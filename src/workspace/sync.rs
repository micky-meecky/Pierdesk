//! Full-snapshot replication with durable local commits and per-edge handshakes.
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::cross_screen::hub::PeerConnectionHub;
use crate::cross_screen::{ApplicationProtocol, ChannelState, PeerPlacement};
use crate::device_store::DeviceStore;

use super::graph::{ConnectionGraph, TrustAdvertisement};
use super::persistence::save_layout;
use super::{
    DeviceId, Edge, GridPoint, LayoutError, LayoutRevision, SnapshotOrder, WorkspaceLayout, edge_id,
};

const MAX_SYNC_MESSAGE: usize = 16 * 1024 - 1;
const PREPARE_TIMEOUT: Duration = Duration::from_secs(5);
const HELLO_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutHello {
    pub workspace_id: [u8; 16],
    pub revision: LayoutRevision,
    pub digest: [u8; 32],
    pub edge_id: [u8; 32],
    /// Sender's direction to receiver, derived from its complete snapshot.
    pub direction: Option<GridPoint>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
enum SyncMessage {
    Trust {
        advertisement: TrustAdvertisement,
    },
    Hello {
        hello: LayoutHello,
    },
    Prepare {
        transaction: [u8; 16],
        base: LayoutRevision,
        snapshot: String,
    },
    Prepared {
        transaction: [u8; 16],
        revision: LayoutRevision,
        digest: [u8; 32],
        accepted: bool,
    },
    Snapshot {
        snapshot: String,
    },
    RequestSnapshot,
}

struct PendingEdit {
    transaction: [u8; 16],
    candidate: WorkspaceLayout,
    awaiting: BTreeSet<DeviceId>,
    deadline: Instant,
}

/// Serialized by the product coordinator. No network handler can enable an edge
/// until a complete durable snapshot and reciprocal hello have been observed.
pub struct LayoutReplicator {
    local: DeviceId,
    layout: WorkspaceLayout,
    members: BTreeSet<DeviceId>,
    trusted_edges: BTreeSet<Edge>,
    graph: ConnectionGraph,
    trust_revision: u64,
    next_trust_check: Instant,
    hellos: BTreeMap<DeviceId, LayoutHello>,
    generations: BTreeMap<DeviceId, u64>,
    pending: Option<PendingEdit>,
    prepared_until: BTreeMap<DeviceId, Instant>,
    next_hello: Instant,
    last_error: Option<String>,
}

impl LayoutReplicator {
    /// Build the native settings preview against this exact snapshot and trust graph.
    ///
    /// # Errors
    /// Rejects invalid moves or a concurrent pending edit.
    pub fn preview_placement(
        &self,
        peer: DeviceId,
        direction: PeerPlacement,
    ) -> io::Result<super::editor::LayoutPreview> {
        if self.pending.is_some() {
            return Err(invalid("layout edit is already pending"));
        }
        super::editor::LayoutPreview::at_neighbor(
            &self.layout,
            self.local,
            peer,
            direction,
            &self.members,
            &self.trusted_edges,
        )
        .map_err(invalid)
    }

    /// Submit a reviewed preview after checking for intervening layout/trust changes.
    ///
    /// # Errors
    /// Rejects a stale preview or propagates release and transport failures.
    pub fn confirm_preview(
        &mut self,
        hub: &PeerConnectionHub,
        preview: &super::editor::LayoutPreview,
        release_input: &mut impl FnMut() -> io::Result<()>,
    ) -> io::Result<()> {
        let candidate = preview
            .confirm(&self.layout, &self.members, &self.trusted_edges)
            .map_err(invalid)?;
        self.propose(hub, candidate, release_input)
    }
    /// Create replication state after loading a validated workspace.
    ///
    /// # Errors
    /// Rejects missing local membership or layout inconsistent with the connection graph.
    pub fn new(
        local: DeviceId,
        layout: WorkspaceLayout,
        members: BTreeSet<DeviceId>,
        trusted_edges: BTreeSet<Edge>,
    ) -> io::Result<Self> {
        if !members.contains(&local) {
            return Err(invalid("local device is not a workspace member"));
        }
        WorkspaceLayout::decode(&layout.encode().map_err(invalid)?, &members, &trusted_edges)
            .map_err(invalid)?;
        let local_trust = trusted_edges
            .iter()
            .filter_map(|(a, b)| {
                if *a == local {
                    Some(*b)
                } else if *b == local {
                    Some(*a)
                } else {
                    None
                }
            })
            .collect();
        let graph = ConnectionGraph::new(local, members.clone(), local_trust).map_err(invalid)?;
        Ok(Self {
            local,
            layout,
            members,
            trusted_edges,
            graph,
            trust_revision: 1,
            next_trust_check: Instant::now(),
            hellos: BTreeMap::new(),
            generations: BTreeMap::new(),
            pending: None,
            prepared_until: BTreeMap::new(),
            next_hello: Instant::now(),
            last_error: None,
        })
    }

    #[must_use]
    pub fn layout(&self) -> &WorkspaceLayout {
        &self.layout
    }

    #[must_use]
    pub fn members(&self) -> &BTreeSet<DeviceId> {
        &self.members
    }

    #[must_use]
    pub fn trusted_edges(&self) -> &BTreeSet<Edge> {
        &self.trusted_edges
    }

    #[must_use]
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    #[must_use]
    pub fn pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Full map and per-edge readiness for the settings UI; local centering is a view only.
    #[must_use]
    pub fn settings_model(&self) -> serde_json::Value {
        use crate::security::CertificateFingerprint;
        let view = self.layout.local_view(self.local).unwrap_or_default();
        let devices: Vec<_> = self
            .layout
            .positions()
            .iter()
            .map(|(id, point)| {
                let direction = self.layout.direction(self.local, *id);
                let local_view = view.get(id);
                serde_json::json!({
                    "fingerprint": CertificateFingerprint::from_bytes(*id).to_string(),
                    "x": point.x, "y": point.y,
                    "localX": local_view.map(|p| p.0), "localY": local_view.map(|p| p.1),
                    "isLocal": *id == self.local, "direction": direction.map(PeerPlacement::label),
                    "online": *id == self.local || self.generations.contains_key(id),
                    "edgeReady": self.edge_ready(*id),
                })
            })
            .collect();
        serde_json::json!({
            "workspaceId": self.layout.workspace_id(), "revision": self.layout.revision().counter,
            "writer": CertificateFingerprint::from_bytes(self.layout.revision().writer).to_string(),
            "digest": self.layout.digest(), "pending": self.pending(), "lastError": self.last_error(),
            "devices": devices,
        })
    }

    #[must_use]
    pub fn edge_ready(&self, peer: DeviceId) -> bool {
        if self.pending.is_some()
            || self
                .prepared_until
                .values()
                .any(|deadline| Instant::now() < *deadline)
            || !self.generations.contains_key(&peer)
        {
            return false;
        }
        self.hellos.get(&peer).is_some_and(|hello| {
            self.graph
                .trusted_edges()
                .contains(&super::edge(self.local, peer))
                && hello.workspace_id == self.layout.workspace_id()
                && hello.revision == self.layout.revision()
                && hello.digest == self.layout.digest()
                && hello.edge_id == edge_id(self.local, peer)
                && self
                    .layout
                    .direction(self.local, peer)
                    .is_some_and(|direction| hello.direction == Some(delta(direction.opposite())))
        })
    }

    /// Begin an edit after the UI has reviewed its complete edge impact.
    ///
    /// # Errors
    /// Rejects concurrent local edits, stale candidates or failure to release input.
    pub fn propose(
        &mut self,
        hub: &PeerConnectionHub,
        candidate: WorkspaceLayout,
        release_input: &mut impl FnMut() -> io::Result<()>,
    ) -> io::Result<()> {
        if self.pending.is_some() {
            return Err(invalid("another layout edit is awaiting acknowledgement"));
        }
        if candidate.revision().writer != self.local
            || self.layout.revision().counter.checked_add(1) != Some(candidate.revision().counter)
            || self.layout.compare(&candidate).map_err(invalid)? != SnapshotOrder::Newer
        {
            return Err(invalid(
                "layout proposal must be a newer locally authored snapshot",
            ));
        }
        WorkspaceLayout::decode(
            &candidate.encode().map_err(invalid)?,
            &self.members,
            &self.trusted_edges,
        )
        .map_err(invalid)?;
        release_input()?;
        let mut transaction = [0; 16];
        getrandom::fill(&mut transaction).map_err(invalid)?;
        if transaction == [0; 16] {
            transaction[0] = 1;
        }
        let peers = online_peers(hub);
        let message = SyncMessage::Prepare {
            transaction,
            base: self.layout.revision(),
            snapshot: snapshot_text(&candidate)?,
        };
        // Queue failures leave the local snapshot unchanged. Periodic hellos recover
        // any receiver which temporarily disabled an edge after a partial prepare.
        for peer in peers.keys() {
            send(hub, *peer, &message)?;
        }
        self.pending = Some(PendingEdit {
            transaction,
            candidate,
            awaiting: peers.keys().copied().collect(),
            deadline: Instant::now() + PREPARE_TIMEOUT,
        });
        self.last_error = None;
        Ok(())
    }

    /// Pump bounded per-peer queues; call from the sole product coordinator.
    /// `release_input` must synchronously return local and release held input before saving.
    ///
    /// # Errors
    /// Propagates local persistence/release failures. Malformed remote messages isolate
    /// layout readiness for that peer and do not stop unrelated links or SSH.
    pub fn tick(
        &mut self,
        store: &DeviceStore,
        hub: &PeerConnectionHub,
        release_input: &mut impl FnMut() -> io::Result<()>,
    ) -> io::Result<()> {
        self.refresh_local_trust(store)?;
        self.tick_connections(store, hub, release_input)
    }

    fn refresh_local_trust(&mut self, store: &DeviceStore) -> io::Result<()> {
        if Instant::now() < self.next_trust_check {
            return Ok(());
        }
        let local_trust = store
            .list_peers()?
            .into_iter()
            .map(|peer| peer.fingerprint.bytes())
            .filter(|peer| self.members.contains(peer))
            .collect();
        if self
            .graph
            .replace_local_trust(local_trust)
            .map_err(invalid)?
        {
            self.trust_revision = self
                .trust_revision
                .checked_add(1)
                .filter(|value| *value != u64::MAX)
                .ok_or_else(|| invalid("trust revision exhausted"))?;
            self.next_hello = Instant::now();
            self.trusted_edges = self.graph.trusted_edges();
        }
        self.next_trust_check = Instant::now() + HELLO_INTERVAL;
        Ok(())
    }

    fn tick_connections(
        &mut self,
        store: &DeviceStore,
        hub: &PeerConnectionHub,
        release_input: &mut impl FnMut() -> io::Result<()>,
    ) -> io::Result<()> {
        let online = online_peers(hub);
        self.hellos.retain(|peer, _| {
            self.generations.get(peer) == online.get(peer) && online.contains_key(peer)
        });
        let changed = self.generations != online;
        self.generations = online;
        self.prepared_until.retain(|peer, deadline| {
            self.generations.contains_key(peer) && Instant::now() < *deadline
        });
        let peers = self.generations.keys().copied().collect::<Vec<_>>();
        for peer in &peers {
            for _ in 0..8 {
                let Ok(Some(bytes)) = hub.with_peer(*peer, |c| {
                    c.try_receive_application(ApplicationProtocol::Layout)
                }) else {
                    break;
                };
                let result = decode(&bytes)
                    .and_then(|message| self.receive(store, hub, *peer, message, release_input));
                if let Err(error) = result {
                    self.hellos.remove(peer);
                    self.last_error = Some(format!("layout peer {peer:?}: {error}"));
                    // Persistence failures must be surfaced to the product host as well.
                    if error.kind() != io::ErrorKind::InvalidData {
                        return Err(error);
                    }
                }
            }
        }
        if let Some(pending) = &mut self.pending {
            pending
                .awaiting
                .retain(|peer| self.generations.contains_key(peer));
            if Instant::now() >= pending.deadline {
                self.pending = None;
                self.last_error =
                    Some("layout acknowledgement timed out; previous layout retained".into());
                self.next_hello = Instant::now();
            } else if pending.awaiting.is_empty() {
                let candidate = pending.candidate.clone();
                self.commit(store, candidate, release_input)?;
                self.pending = None;
                self.broadcast(
                    hub,
                    &SyncMessage::Snapshot {
                        snapshot: snapshot_text(&self.layout)?,
                    },
                );
            }
        }
        if changed || Instant::now() >= self.next_hello {
            for peer in peers {
                let _ = send(
                    hub,
                    peer,
                    &SyncMessage::Trust {
                        advertisement: self.graph.advertisement(self.trust_revision),
                    },
                );
                let _ = send(
                    hub,
                    peer,
                    &SyncMessage::Hello {
                        hello: self.hello(peer),
                    },
                );
            }
            self.next_hello = Instant::now() + HELLO_INTERVAL;
        }
        Ok(())
    }

    fn hello(&self, peer: DeviceId) -> LayoutHello {
        LayoutHello {
            workspace_id: self.layout.workspace_id(),
            revision: self.layout.revision(),
            digest: self.layout.digest(),
            edge_id: edge_id(self.local, peer),
            direction: self.layout.direction(self.local, peer).map(delta),
        }
    }

    fn broadcast(&self, hub: &PeerConnectionHub, message: &SyncMessage) {
        for peer in self.generations.keys() {
            let _ = send(hub, *peer, message);
        }
    }

    fn commit(
        &mut self,
        store: &DeviceStore,
        candidate: WorkspaceLayout,
        release_input: &mut impl FnMut() -> io::Result<()>,
    ) -> io::Result<()> {
        release_input().map_err(io::Error::other)?;
        save_layout(store, &candidate).map_err(io::Error::other)?;
        self.layout = candidate;
        self.prepared_until.clear();
        self.hellos.clear();
        self.next_hello = Instant::now();
        Ok(())
    }

    fn receive_hello(
        &mut self,
        hub: &PeerConnectionHub,
        peer: DeviceId,
        hello: LayoutHello,
    ) -> io::Result<()> {
        if hello.workspace_id != self.layout.workspace_id()
            || hello.edge_id != edge_id(self.local, peer)
        {
            return Err(invalid("layout hello workspace or edge mismatch"));
        }
        let order = hello.revision.cmp(&self.layout.revision());
        if order == std::cmp::Ordering::Equal && hello.digest != self.layout.digest() {
            return Err(invalid(LayoutError::DigestMismatch));
        }
        self.hellos.insert(peer, hello);
        match order {
            std::cmp::Ordering::Greater => {
                let _ = send(hub, peer, &SyncMessage::RequestSnapshot);
            }
            std::cmp::Ordering::Less => {
                let _ = send(
                    hub,
                    peer,
                    &SyncMessage::Snapshot {
                        snapshot: snapshot_text(&self.layout)?,
                    },
                );
            }
            std::cmp::Ordering::Equal => {}
        }
        Ok(())
    }

    fn receive_prepared(
        &mut self,
        peer: DeviceId,
        transaction: [u8; 16],
        revision: LayoutRevision,
        digest: [u8; 32],
        accepted: bool,
    ) {
        if let Some(pending) = &mut self.pending
            && pending.transaction == transaction
            && pending.candidate.revision() == revision
            && pending.candidate.digest() == digest
            && pending.awaiting.contains(&peer)
        {
            if accepted {
                pending.awaiting.remove(&peer);
            } else {
                self.pending = None;
                self.last_error = Some(
                    "layout was updated by another device; review the latest layout and retry"
                        .into(),
                );
                self.next_hello = Instant::now();
            }
        }
    }

    fn receive(
        &mut self,
        store: &DeviceStore,
        hub: &PeerConnectionHub,
        peer: DeviceId,
        message: SyncMessage,
        release_input: &mut impl FnMut() -> io::Result<()>,
    ) -> io::Result<()> {
        if !self.members.contains(&peer) {
            return Err(invalid("layout sender is not a member"));
        }
        match message {
            SyncMessage::Trust { advertisement } => {
                let generation = self
                    .generations
                    .get(&peer)
                    .copied()
                    .ok_or_else(|| invalid("trust sender is offline"))?;
                self.graph
                    .observe(peer, generation, &advertisement)
                    .map_err(invalid)?;
                self.trusted_edges = self.graph.trusted_edges();
            }
            SyncMessage::Hello { hello } => self.receive_hello(hub, peer, hello)?,
            SyncMessage::RequestSnapshot => {
                let _ = send(
                    hub,
                    peer,
                    &SyncMessage::Snapshot {
                        snapshot: snapshot_text(&self.layout)?,
                    },
                );
            }
            SyncMessage::Prepare {
                transaction,
                base,
                snapshot,
            } => {
                let candidate = WorkspaceLayout::decode(
                    snapshot.as_bytes(),
                    &self.members,
                    &self.trusted_edges,
                )
                .map_err(invalid)?;
                let accepted = transaction != [0; 16]
                    && candidate.revision().writer == peer
                    && base == self.layout.revision()
                    && self.layout.compare(&candidate).map_err(invalid)? == SnapshotOrder::Newer;
                if accepted {
                    release_input().map_err(io::Error::other)?;
                    self.hellos.remove(&peer);
                    self.prepared_until
                        .insert(peer, Instant::now() + PREPARE_TIMEOUT);
                }
                let _ = send(
                    hub,
                    peer,
                    &SyncMessage::Prepared {
                        transaction,
                        revision: candidate.revision(),
                        digest: candidate.digest(),
                        accepted,
                    },
                );
            }
            SyncMessage::Prepared {
                transaction,
                revision,
                digest,
                accepted,
            } => self.receive_prepared(peer, transaction, revision, digest, accepted),
            SyncMessage::Snapshot { snapshot } => {
                let candidate = WorkspaceLayout::decode(
                    snapshot.as_bytes(),
                    &self.members,
                    &self.trusted_edges,
                )
                .map_err(invalid)?;
                match self.layout.compare(&candidate).map_err(invalid)? {
                    SnapshotOrder::Newer => {
                        self.commit(store, candidate, release_input)?;
                        if self.pending.as_ref().is_some_and(|pending| {
                            pending.candidate.revision() <= self.layout.revision()
                        }) {
                            self.pending = None;
                            self.last_error = Some("layout was updated by another device; review the latest layout and retry".into());
                        }
                        self.broadcast(
                            hub,
                            &SyncMessage::Snapshot {
                                snapshot: snapshot_text(&self.layout)?,
                            },
                        );
                    }
                    SnapshotOrder::Older => {
                        let _ = send(
                            hub,
                            peer,
                            &SyncMessage::Snapshot {
                                snapshot: snapshot_text(&self.layout)?,
                            },
                        );
                    }
                    SnapshotOrder::Equal => {}
                }
            }
        }
        Ok(())
    }
}

fn online_peers(hub: &PeerConnectionHub) -> BTreeMap<DeviceId, u64> {
    hub.snapshots()
        .into_iter()
        .filter(|p| {
            p.channel
                .as_ref()
                .is_some_and(|c| c.state == ChannelState::Connected)
        })
        .map(|p| (p.peer, p.generation))
        .collect()
}

fn snapshot_text(snapshot: &WorkspaceLayout) -> io::Result<String> {
    String::from_utf8(snapshot.encode().map_err(invalid)?).map_err(invalid)
}
fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

fn decode(bytes: &[u8]) -> io::Result<SyncMessage> {
    if bytes.len() > MAX_SYNC_MESSAGE {
        return Err(invalid("layout message exceeds limit"));
    }
    serde_json::from_slice(bytes).map_err(invalid)
}

fn send(hub: &PeerConnectionHub, peer: DeviceId, message: &SyncMessage) -> io::Result<()> {
    let bytes = serde_json::to_vec(message).map_err(invalid)?;
    if bytes.len() > MAX_SYNC_MESSAGE {
        return Err(invalid("layout message exceeds limit"));
    }
    hub.with_peer(peer, |c| {
        c.send_application(ApplicationProtocol::Layout, bytes)
    })
}

const fn delta(direction: PeerPlacement) -> GridPoint {
    match direction {
        PeerPlacement::Left => GridPoint { x: -1, y: 0 },
        PeerPlacement::Right => GridPoint { x: 1, y: 0 },
        PeerPlacement::Above => GridPoint { x: 0, y: -1 },
        PeerPlacement::Below => GridPoint { x: 0, y: 1 },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cross_screen::hub::tests::Network;

    fn replicas(network: &Network) -> Vec<LayoutReplicator> {
        let members: BTreeSet<_> = network.ids.iter().copied().collect();
        let positions = BTreeMap::from([
            (network.ids[0], GridPoint { x: 0, y: 0 }),
            (network.ids[1], GridPoint { x: 0, y: -1 }),
            (network.ids[2], GridPoint { x: 1, y: 0 }),
        ]);
        let edges = members
            .iter()
            .flat_map(|a| {
                members
                    .iter()
                    .filter(move |b| a < *b)
                    .map(move |b| (*a, *b))
            })
            .collect::<BTreeSet<_>>();
        let layout = WorkspaceLayout::new(
            [8; 16],
            LayoutRevision {
                counter: 1,
                writer: network.ids[0],
            },
            &positions,
            &members,
            &edges,
        )
        .unwrap();
        network
            .ids
            .iter()
            .enumerate()
            .map(|(i, id)| {
                save_layout(&network.stores[i], &layout).unwrap();
                LayoutReplicator::new(*id, layout.clone(), members.clone(), edges.clone()).unwrap()
            })
            .collect()
    }

    fn pump_until(
        network: &Network,
        replicas: &mut [LayoutReplicator],
        mut predicate: impl FnMut(&[LayoutReplicator]) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            for (i, replica) in replicas.iter_mut().enumerate() {
                replica
                    .tick(&network.stores[i], &network.hubs[i], &mut || Ok(()))
                    .unwrap();
            }
            if predicate(replicas) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "layout convergence timed out: {:?}",
                replicas
                    .iter()
                    .map(LayoutReplicator::last_error)
                    .collect::<Vec<_>>()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn all_edges_ready(network: &Network, replicas: &[LayoutReplicator]) -> bool {
        replicas.iter().enumerate().all(|(i, r)| {
            network
                .ids
                .iter()
                .all(|id| r.layout.direction(network.ids[i], *id).is_none() || r.edge_ready(*id))
        })
    }

    #[test]
    fn full_swap_is_persisted_and_reciprocal_edges_enable_only_after_handshakes() {
        let network = Network::new(3);
        network.wait_connected();
        let mut replicas = replicas(&network);
        assert!(!replicas[0].edge_ready(network.ids[1]));
        pump_until(&network, &mut replicas, |r| all_edges_ready(&network, r));
        let target = replicas[0].layout.positions()[&network.ids[2]];
        let candidate = replicas[0]
            .layout
            .place(
                network.ids[1],
                target,
                network.ids[0],
                &replicas[0].members,
                &replicas[0].trusted_edges,
            )
            .unwrap()
            .candidate;
        let mut releases = 0;
        replicas[0]
            .propose(&network.hubs[0], candidate.clone(), &mut || {
                releases += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(releases, 1);
        assert!(!replicas[0].edge_ready(network.ids[1]));
        pump_until(&network, &mut replicas, |r| {
            r.iter().all(|r| r.layout == candidate) && all_edges_ready(&network, r)
        });
        for (i, replica) in replicas.iter().enumerate() {
            let persisted =
                std::fs::read(network.stores[i].root().join("workspace-layout.v1")).unwrap();
            assert_eq!(
                WorkspaceLayout::decode(&persisted, &replica.members, &replica.trusted_edges)
                    .unwrap(),
                candidate
            );
        }
    }

    #[test]
    fn revision_digest_edge_and_direction_mismatches_disable_only_that_peer() {
        let network = Network::new(3);
        network.wait_connected();
        let mut replicas = replicas(&network);
        pump_until(&network, &mut replicas, |r| all_edges_ready(&network, r));
        for field in 0..4 {
            let mut hello = replicas[1].hello(network.ids[0]);
            match field {
                0 => hello.revision.counter += 1,
                1 => hello.digest[0] ^= 1,
                2 => hello.edge_id[0] ^= 1,
                _ => hello.direction = Some(GridPoint { x: 1, y: 1 }),
            }
            // Same error handling as tick: peer readiness is cleared on rejected input.
            if replicas[0]
                .receive(
                    &network.stores[0],
                    &network.hubs[0],
                    network.ids[1],
                    SyncMessage::Hello { hello },
                    &mut || Ok(()),
                )
                .is_err()
            {
                replicas[0].hellos.remove(&network.ids[1]);
            }
            assert!(!replicas[0].edge_ready(network.ids[1]));
            assert!(replicas[0].edge_ready(network.ids[2]));
            let hello = replicas[1].hello(network.ids[0]);
            replicas[0].hellos.insert(network.ids[1], hello);
        }
    }

    #[test]
    fn partitioned_full_snapshots_converge_to_revision_winner_without_field_merging() {
        let network = Network::new(3);
        network.wait_connected();
        let mut replicas = replicas(&network);
        let a = replicas[0]
            .layout
            .place(
                network.ids[1],
                replicas[0].layout.positions()[&network.ids[2]],
                network.ids[0],
                &replicas[0].members,
                &replicas[0].trusted_edges,
            )
            .unwrap()
            .candidate;
        let b = replicas[1]
            .layout
            .place(
                network.ids[2],
                replicas[1].layout.positions()[&network.ids[0]],
                network.ids[1],
                &replicas[1].members,
                &replicas[1].trusted_edges,
            )
            .unwrap()
            .candidate;
        // Independent durable commits made before either partition has received the other.
        replicas[0]
            .commit(&network.stores[0], a.clone(), &mut || Ok(()))
            .unwrap();
        replicas[1]
            .commit(&network.stores[1], b.clone(), &mut || Ok(()))
            .unwrap();
        let winner = if a.revision() > b.revision() { a } else { b };
        pump_until(&network, &mut replicas, |r| {
            r.iter().all(|r| r.layout == winner) && all_edges_ready(&network, r)
        });
    }

    #[test]
    fn simultaneous_prepares_resolve_to_the_same_revision_winner() {
        let network = Network::new(3);
        network.wait_connected();
        let mut replicas = replicas(&network);
        pump_until(&network, &mut replicas, |r| all_edges_ready(&network, r));
        let a = replicas[0]
            .layout
            .place(
                network.ids[1],
                replicas[0].layout.positions()[&network.ids[2]],
                network.ids[0],
                &replicas[0].members,
                &replicas[0].trusted_edges,
            )
            .unwrap()
            .candidate;
        let b = replicas[1]
            .layout
            .place(
                network.ids[2],
                replicas[1].layout.positions()[&network.ids[0]],
                network.ids[1],
                &replicas[1].members,
                &replicas[1].trusted_edges,
            )
            .unwrap()
            .candidate;
        let winner = if a.revision() > b.revision() {
            a.clone()
        } else {
            b.clone()
        };
        replicas[0]
            .propose(&network.hubs[0], a, &mut || Ok(()))
            .unwrap();
        replicas[1]
            .propose(&network.hubs[1], b, &mut || Ok(()))
            .unwrap();
        pump_until(&network, &mut replicas, |r| {
            r.iter().all(|r| r.layout == winner && !r.pending()) && all_edges_ready(&network, r)
        });
    }

    #[test]
    fn authenticated_remote_revocation_removes_only_its_connection_edge() {
        let network = Network::new(3);
        network.wait_connected();
        let mut replicas = replicas(&network);
        let remote_edge = super::super::edge(network.ids[1], network.ids[2]);
        pump_until(&network, &mut replicas, |r| {
            all_edges_ready(&network, r) && r[0].trusted_edges.contains(&remote_edge)
        });
        network.stores[1]
            .revoke_peer(crate::security::CertificateFingerprint::from_bytes(
                network.ids[2],
            ))
            .unwrap();
        pump_until(&network, &mut replicas, |r| {
            !r[0].trusted_edges.contains(&remote_edge) && all_edges_ready(&network, r)
        });
        let neighbor = replicas[0].layout.positions()[&network.ids[1]];
        assert_eq!(
            replicas[0].layout.place(
                network.ids[2],
                GridPoint {
                    x: neighbor.x + 1,
                    y: neighbor.y
                },
                network.ids[0],
                &replicas[0].members,
                &replicas[0].trusted_edges
            ),
            Err(LayoutError::MissingTrustedEdge)
        );
        assert!(replicas[0].edge_ready(network.ids[1]));
        assert!(replicas[0].edge_ready(network.ids[2]));
    }

    #[test]
    fn failed_release_or_persistence_keeps_complete_previous_snapshot() {
        let network = Network::new(3);
        network.wait_connected();
        let mut replicas = replicas(&network);
        let original = replicas[0].layout.clone();
        let candidate = original
            .place(
                network.ids[1],
                original.positions()[&network.ids[2]],
                network.ids[0],
                &replicas[0].members,
                &replicas[0].trusted_edges,
            )
            .unwrap()
            .candidate;
        assert!(
            replicas[0]
                .propose(&network.hubs[0], candidate.clone(), &mut || Err(
                    io::Error::other("release failed")
                ))
                .is_err()
        );
        assert!(!replicas[0].pending());
        assert_eq!(replicas[0].layout, original);
        let invalid_root = network.root.join("not-a-directory");
        std::fs::write(&invalid_root, b"blocked").unwrap();
        assert!(
            replicas[0]
                .commit(&DeviceStore::new(invalid_root), candidate, &mut || Ok(()))
                .is_err()
        );
        assert_eq!(replicas[0].layout, original);
    }

    #[test]
    fn stale_prepare_is_rejected_and_equal_revision_conflict_does_not_replace_disk() {
        let network = Network::new(3);
        network.wait_connected();
        let mut replicas = replicas(&network);
        let original = replicas[0].layout.clone();
        let candidate = original
            .place(
                network.ids[1],
                original.positions()[&network.ids[2]],
                network.ids[1],
                &replicas[0].members,
                &replicas[0].trusted_edges,
            )
            .unwrap()
            .candidate;
        replicas[0]
            .receive(
                &network.stores[0],
                &network.hubs[0],
                network.ids[1],
                SyncMessage::Prepare {
                    transaction: [1; 16],
                    base: LayoutRevision {
                        counter: 0,
                        writer: network.ids[0],
                    },
                    snapshot: snapshot_text(&candidate).unwrap(),
                },
                &mut || panic!("stale proposal must not release input"),
            )
            .unwrap();
        assert!(replicas[0].prepared_until.is_empty());
        let conflict = WorkspaceLayout::new(
            original.workspace_id(),
            original.revision(),
            candidate.positions(),
            &replicas[0].members,
            &replicas[0].trusted_edges,
        )
        .unwrap();
        assert!(
            replicas[0]
                .receive(
                    &network.stores[0],
                    &network.hubs[0],
                    network.ids[1],
                    SyncMessage::Snapshot {
                        snapshot: snapshot_text(&conflict).unwrap()
                    },
                    &mut || panic!("conflicting snapshot must not commit")
                )
                .is_err()
        );
        assert_eq!(replicas[0].layout, original);
    }

    #[test]
    fn offline_edit_commits_locally_and_latest_snapshot_recovers_other_peers() {
        let network = Network::new(3);
        network.wait_connected();
        let mut replicas = replicas(&network);
        let isolated = PeerConnectionHub::from_listener(
            network.stores[0].clone(),
            std::net::TcpListener::bind("127.0.0.1:0").unwrap(),
            BTreeMap::new(),
        )
        .unwrap();
        let candidate = replicas[0]
            .layout
            .place(
                network.ids[1],
                replicas[0].layout.positions()[&network.ids[2]],
                network.ids[0],
                &replicas[0].members,
                &replicas[0].trusted_edges,
            )
            .unwrap()
            .candidate;
        replicas[0]
            .propose(&isolated, candidate.clone(), &mut || Ok(()))
            .unwrap();
        replicas[0]
            .tick(&network.stores[0], &isolated, &mut || Ok(()))
            .unwrap();
        assert_eq!(replicas[0].layout, candidate);
        assert!(!replicas[0].pending());
        assert!(!replicas[0].edge_ready(network.ids[1]));
        pump_until(&network, &mut replicas, |r| {
            r.iter().all(|r| r.layout == candidate) && all_edges_ready(&network, r)
        });
    }

    #[test]
    fn missing_ack_retains_old_layout_and_prepare_blocks_old_hello_until_expiry() {
        let network = Network::new(3);
        network.wait_connected();
        let mut replicas = replicas(&network);
        pump_until(&network, &mut replicas, |r| all_edges_ready(&network, r));
        let original = replicas[0].layout.clone();
        let candidate = original
            .place(
                network.ids[1],
                original.positions()[&network.ids[2]],
                network.ids[0],
                &replicas[0].members,
                &replicas[0].trusted_edges,
            )
            .unwrap()
            .candidate;
        replicas[0]
            .propose(&network.hubs[0], candidate, &mut || Ok(()))
            .unwrap();
        replicas[0].pending.as_mut().unwrap().deadline = Instant::now();
        replicas[0]
            .tick(&network.stores[0], &network.hubs[0], &mut || Ok(()))
            .unwrap();
        assert!(!replicas[0].pending());
        assert_eq!(replicas[0].layout, original);
        let deadline = Instant::now() + Duration::from_secs(3);
        while replicas[1].prepared_until.is_empty() {
            replicas[1]
                .tick(&network.stores[1], &network.hubs[1], &mut || Ok(()))
                .unwrap();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        let hello = replicas[0].hello(network.ids[1]);
        replicas[1]
            .receive(
                &network.stores[1],
                &network.hubs[1],
                network.ids[0],
                SyncMessage::Hello { hello },
                &mut || Ok(()),
            )
            .unwrap();
        assert!(!replicas[1].edge_ready(network.ids[0]));
        replicas[1]
            .prepared_until
            .insert(network.ids[0], Instant::now());
        assert!(replicas[1].edge_ready(network.ids[0]));
    }
}
