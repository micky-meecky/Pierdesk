//! Versioned workspace configuration and non-destructive two-device migration.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::net::SocketAddr;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::app_settings::{AppSettings, CrossScreenSettings, validate_private_endpoint};
use crate::cross_screen::PeerPlacement;
use crate::device_store::DeviceStore;
use crate::security::CertificateFingerprint;

use super::{DeviceId, Edge, GridPoint, LayoutRevision, MAX_LAYOUT_BYTES, WorkspaceLayout, edge};

const CONNECTIONS_FILE: &str = "connections.v2";
const LAYOUT_FILE: &str = "workspace-layout.v1";
const BEHAVIOR_FILE: &str = "cross-screen.v2";
const MIGRATION_FILE: &str = "workspace-migration.v1";
const CONTROL_EPOCH_FILE: &str = "control-epoch.v1";
const CONTROL_EPOCH_BLOCK: u64 = 1 << 32;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerConnectionSettings {
    pub peer: DeviceId,
    pub last_endpoint: SocketAddr,
    pub manual_endpoint: Option<SocketAddr>,
    pub enabled: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionSettings {
    pub schema: u32,
    pub local_endpoint: SocketAddr,
    pub peers: Vec<PeerConnectionSettings>,
    pub auto_receive: bool,
}

/// These preferences belong to this computer and never participate in layout sync.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)]
pub struct LocalBoundarySettings {
    pub schema: u32,
    pub enabled: bool,
    pub corner_guard_enabled: bool,
    pub corner_guard_pixels: u32,
    pub require_second_push: bool,
    pub pause_on_fullscreen: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceConfiguration {
    pub connections: ConnectionSettings,
    pub layout: WorkspaceLayout,
    pub behavior: LocalBoundarySettings,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MigrationRecord {
    schema: u32,
    connections_digest: [u8; 32],
    layout_digest: [u8; 32],
    behavior_digest: [u8; 32],
    runtime_started: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlEpochRecord {
    schema: u32,
    next_floor: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControlEpochRange {
    pub floor: u64,
    pub limit: u64,
}

impl ConnectionSettings {
    /// Check member limits, trust and independent endpoints, without opening sockets.
    ///
    /// # Errors
    /// Rejects invalid configuration, duplicate peers or revoked trust.
    pub fn validate(&self, store: &DeviceStore) -> io::Result<()> {
        if self.schema != 2 || self.peers.len() > 4 {
            return Err(invalid("invalid connection schema or peer count"));
        }
        validate_private_endpoint(self.local_endpoint)?;
        let local = store.load_identity()?.fingerprint().bytes();
        let mut seen = BTreeSet::new();
        for peer in &self.peers {
            if peer.peer == local || !seen.insert(peer.peer) {
                return Err(invalid("duplicate or local peer"));
            }
            store.load_peer(CertificateFingerprint::from_bytes(peer.peer))?;
            for endpoint in std::iter::once(peer.last_endpoint).chain(peer.manual_endpoint) {
                validate_private_endpoint(endpoint)?;
                if endpoint.ip().is_loopback() != self.local_endpoint.ip().is_loopback() {
                    return Err(invalid(
                        "connection endpoints have different network scopes",
                    ));
                }
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn members(&self, local: DeviceId) -> BTreeSet<DeviceId> {
        std::iter::once(local)
            .chain(self.peers.iter().map(|peer| peer.peer))
            .collect()
    }
}

impl WorkspaceConfiguration {
    /// Read and validate legacy files; produce the new model without modifying disk.
    ///
    /// # Errors
    /// Legacy parse/trust errors abort migration and leave all old files untouched.
    pub fn from_legacy(store: &DeviceStore) -> io::Result<Self> {
        let app = AppSettings::load(store)?;
        let cross = CrossScreenSettings::load_or_default(store)?;
        let local = store.load_identity()?.fingerprint().bytes();
        let peer = app.active_peer.bytes();
        // The legacy listener was the SMALLER fingerprint. New dialer selection must
        // not change the interpretation of already saved listener-relative placement.
        let placement = cross.local_peer_placement(local < peer);
        let point = match placement {
            PeerPlacement::Left => GridPoint { x: -1, y: 0 },
            PeerPlacement::Right => GridPoint { x: 1, y: 0 },
            PeerPlacement::Above => GridPoint { x: 0, y: -1 },
            PeerPlacement::Below => GridPoint { x: 0, y: 1 },
        };
        let connections = ConnectionSettings {
            schema: 2,
            local_endpoint: app.local_endpoint,
            auto_receive: app.auto_receive,
            peers: vec![PeerConnectionSettings {
                peer,
                last_endpoint: app.peer_endpoint,
                manual_endpoint: Some(app.peer_endpoint),
                enabled: true,
            }],
        };
        connections.validate(store)?;
        let pair = edge(local, peer);
        // Both ends independently derive the same workspace ID for an existing pair.
        let mut hash = Sha256::new();
        hash.update(b"ClipFerryLegacyWorkspaceV1\0");
        hash.update(pair.0);
        hash.update(pair.1);
        let digest = hash.finalize();
        let mut workspace_id = [0; 16];
        workspace_id.copy_from_slice(&digest[..16]);
        let layout = WorkspaceLayout::new(
            workspace_id,
            LayoutRevision {
                counter: cross.layout_revision,
                writer: pair.0,
            },
            &BTreeMap::from([(local, GridPoint { x: 0, y: 0 }), (peer, point)]),
            &connections.members(local),
            &BTreeSet::from([pair]),
        )
        .map_err(invalid)?;
        Ok(Self {
            connections,
            layout,
            behavior: LocalBoundarySettings {
                schema: 2,
                enabled: cross.enabled,
                corner_guard_enabled: cross.corner_guard_enabled,
                corner_guard_pixels: cross.corner_guard_pixels,
                require_second_push: cross.require_second_push,
                pause_on_fullscreen: cross.pause_on_fullscreen,
            },
        })
    }

    /// Persist and reread migration output, marking completion only after runtime startup.
    /// Existing new-format files are never overwritten by a legacy migration.
    ///
    /// # Errors
    /// Returns validation, collision or persistence failures; legacy files are preserved.
    pub fn prepare_legacy_migration(store: &DeviceStore) -> io::Result<Self> {
        let candidate = Self::from_legacy(store)?;
        let local = store.load_identity()?.fingerprint().bytes();
        let peer = candidate.connections.peers[0].peer;
        let bytes = [
            (
                CONNECTIONS_FILE,
                serde_json::to_vec(&candidate.connections).map_err(invalid)?,
            ),
            (LAYOUT_FILE, candidate.layout.encode().map_err(invalid)?),
            (
                BEHAVIOR_FILE,
                serde_json::to_vec(&candidate.behavior).map_err(invalid)?,
            ),
        ];
        // Preflight every output before the first mutation. A matching partial migration
        // can be retried after interruption; different data needs explicit recovery.
        for (name, expected) in &bytes {
            match read_bounded(&store.root().join(name)) {
                Ok(existing) if existing != *expected => {
                    return Err(invalid(
                        "new-format configuration already differs from legacy migration",
                    ));
                }
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
        }
        if store.root().join(MIGRATION_FILE).exists() {
            return Err(invalid(
                "migration was already prepared; load the new configuration",
            ));
        }
        for (name, contents) in &bytes {
            if !store.root().join(name).exists() {
                atomic_write(&store.root().join(name), contents, false)?;
            }
        }
        let loaded = Self::load(store, &BTreeSet::from([edge(local, peer)]))?;
        if loaded != candidate {
            return Err(invalid("migration reread differs from candidate"));
        }
        let record = MigrationRecord {
            schema: 1,
            connections_digest: Sha256::digest(&bytes[0].1).into(),
            layout_digest: Sha256::digest(&bytes[1].1).into(),
            behavior_digest: Sha256::digest(&bytes[2].1).into(),
            runtime_started: false,
        };
        atomic_write(
            &store.root().join(MIGRATION_FILE),
            &serde_json::to_vec(&record).map_err(invalid)?,
            false,
        )?;
        Ok(loaded)
    }

    /// Load the three independently versioned configuration files.
    ///
    /// # Errors
    /// Rejects malformed, oversized, untrusted or inconsistent configurations.
    pub fn load(store: &DeviceStore, trusted_edges: &BTreeSet<Edge>) -> io::Result<Self> {
        let connections: ConnectionSettings =
            serde_json::from_slice(&read_bounded(&store.root().join(CONNECTIONS_FILE))?)
                .map_err(invalid)?;
        connections.validate(store)?;
        let behavior: LocalBoundarySettings =
            serde_json::from_slice(&read_bounded(&store.root().join(BEHAVIOR_FILE))?)
                .map_err(invalid)?;
        if behavior.schema != 2 || behavior.corner_guard_pixels > 512 {
            return Err(invalid("invalid local boundary settings"));
        }
        let local = store.load_identity()?.fingerprint().bytes();
        let layout = WorkspaceLayout::decode(
            &read_bounded(&store.root().join(LAYOUT_FILE))?,
            &connections.members(local),
            trusted_edges,
        )
        .map_err(invalid)?;
        Ok(Self {
            connections,
            layout,
            behavior,
        })
    }
}

/// Mark a prepared migration complete only after the caller started the new runtime.
///
/// # Errors
/// Rejects changed outputs, corrupt records and I/O errors.
pub fn mark_migration_started(store: &DeviceStore) -> io::Result<()> {
    let path = store.root().join(MIGRATION_FILE);
    let mut record: MigrationRecord =
        serde_json::from_slice(&read_bounded(&path)?).map_err(invalid)?;
    if record.schema != 1 {
        return Err(invalid("unsupported migration schema"));
    }
    for (name, expected) in [
        (CONNECTIONS_FILE, record.connections_digest),
        (LAYOUT_FILE, record.layout_digest),
        (BEHAVIOR_FILE, record.behavior_digest),
    ] {
        let actual: [u8; 32] = Sha256::digest(read_bounded(&store.root().join(name))?).into();
        if actual != expected {
            return Err(invalid("migration output changed before runtime startup"));
        }
    }
    record.runtime_started = true;
    atomic_write(&path, &serde_json::to_vec(&record).map_err(invalid)?, true)
}

/// Atomically replace one validated layout. Readers see an old or new complete snapshot.
///
/// # Errors
/// Returns encoding and file-system errors without truncating the existing layout.
pub fn save_layout(store: &DeviceStore, layout: &WorkspaceLayout) -> io::Result<()> {
    atomic_write(
        &store.root().join(LAYOUT_FILE),
        &layout.encode().map_err(invalid)?,
        true,
    )
}

/// Reserve one durable range of control epochs for this runtime instance.
///
/// The reservation is written through before the range is returned. A crash can therefore skip a
/// range, but a restarted origin cannot reuse epochs that a still-running target already observed.
///
/// # Errors
/// Rejects corrupt/exhausted state and propagates durable-write failures.
pub fn reserve_control_epoch_range(store: &DeviceStore) -> io::Result<ControlEpochRange> {
    let path = store.root().join(CONTROL_EPOCH_FILE);
    let (floor, replace) = match read_bounded(&path) {
        Ok(bytes) => {
            let record: ControlEpochRecord = serde_json::from_slice(&bytes).map_err(invalid)?;
            if record.schema != 1
                || record.next_floor < CONTROL_EPOCH_BLOCK
                || !record.next_floor.is_multiple_of(CONTROL_EPOCH_BLOCK)
            {
                return Err(invalid("invalid control epoch reservation"));
            }
            (record.next_floor, true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => (CONTROL_EPOCH_BLOCK, false),
        Err(error) => return Err(error),
    };
    let limit = floor
        .checked_add(CONTROL_EPOCH_BLOCK)
        .ok_or_else(|| invalid("control epoch reservation exhausted"))?;
    let record = ControlEpochRecord {
        schema: 1,
        next_floor: limit,
    };
    atomic_write(
        &path,
        &serde_json::to_vec(&record).map_err(invalid)?,
        replace,
    )?;
    Ok(ControlEpochRange { floor, limit })
}

fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

fn read_bounded(path: &Path) -> io::Result<Vec<u8>> {
    let file = OpenOptions::new().read(true).open(path)?;
    if file.metadata()?.len() > MAX_LAYOUT_BYTES as u64 {
        return Err(invalid("workspace file exceeds size limit"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_LAYOUT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.is_empty() || bytes.len() > MAX_LAYOUT_BYTES {
        return Err(invalid("workspace file length is invalid"));
    }
    Ok(bytes)
}

fn atomic_write(path: &Path, bytes: &[u8], replace: bool) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    let parent = path
        .parent()
        .ok_or_else(|| invalid("workspace path has no parent"))?;
    fs::create_dir_all(parent)?;
    let mut random = [0; 8];
    getrandom::fill(&mut random).map_err(invalid)?;
    let temporary = parent.join(format!(
        ".workspace-{}-{:016x}.tmp",
        std::process::id(),
        u64::from_le_bytes(random)
    ));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    let result = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    let result = result.and_then(|()| {
        let from: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
        let to: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let flags = if replace {
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH
        } else {
            MOVEFILE_WRITE_THROUGH
        };
        // Both buffers are NUL-terminated and remain live for the duration of the call.
        unsafe {
            MoveFileExW(
                windows_core::PCWSTR(from.as_ptr()),
                windows_core::PCWSTR(to.as_ptr()),
                flags,
            )
        }
        .map_err(io::Error::other)
    });
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestPair {
        root: std::path::PathBuf,
        a: DeviceStore,
        b: DeviceStore,
    }

    impl TestPair {
        fn new() -> Self {
            let mut random = [0; 8];
            getrandom::fill(&mut random).unwrap();
            let root = std::env::temp_dir().join(format!(
                "pierdesk-workspace-test-{:016x}",
                u64::from_le_bytes(random)
            ));
            let a = DeviceStore::new(root.join("a"));
            let b = DeviceStore::new(root.join("b"));
            let ai = a.load_or_create_identity().unwrap().identity;
            let bi = b.load_or_create_identity().unwrap().identity;
            a.trust_peer(bi.certificate_der().to_vec(), bi.fingerprint(), "b")
                .unwrap();
            b.trust_peer(ai.certificate_der().to_vec(), ai.fingerprint(), "a")
                .unwrap();
            for (local, remote) in [(&a, &b), (&b, &a)] {
                AppSettings {
                    local_endpoint: "127.0.0.1:45233".parse().unwrap(),
                    active_peer: remote.load_identity().unwrap().fingerprint(),
                    peer_endpoint: "127.0.0.2:45233".parse().unwrap(),
                    auto_receive: true,
                }
                .save(local)
                .unwrap();
                CrossScreenSettings {
                    listener_peer_placement: PeerPlacement::Right,
                    layout_revision: 7,
                    corner_guard_pixels: 48,
                    require_second_push: true,
                    ..CrossScreenSettings::default()
                }
                .save(local)
                .unwrap();
            }
            Self { root, a, b }
        }
    }

    impl Drop for TestPair {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn both_legacy_roles_migrate_to_the_same_canonical_layout_in_every_direction() {
        let pair = TestPair::new();
        for placement in [
            PeerPlacement::Left,
            PeerPlacement::Right,
            PeerPlacement::Above,
            PeerPlacement::Below,
        ] {
            for store in [&pair.a, &pair.b] {
                let mut cross = CrossScreenSettings::load(store).unwrap();
                cross.listener_peer_placement = placement;
                cross.save(store).unwrap();
            }
            let a = WorkspaceConfiguration::from_legacy(&pair.a).unwrap();
            let b = WorkspaceConfiguration::from_legacy(&pair.b).unwrap();
            assert_eq!(a.layout, b.layout);
            assert!(a.connections.auto_receive);
            assert_eq!(a.behavior.corner_guard_pixels, 48);
            assert!(a.behavior.require_second_push);
        }
    }

    #[test]
    fn migration_rereads_outputs_and_waits_for_runtime_without_touching_legacy() {
        let pair = TestPair::new();
        let app_before = fs::read(pair.a.root().join("settings.v1")).unwrap();
        let cross_before = fs::read(pair.a.root().join("cross_screen.v1")).unwrap();
        let config = WorkspaceConfiguration::prepare_legacy_migration(&pair.a).unwrap();
        let path = pair.a.root().join(MIGRATION_FILE);
        let record: MigrationRecord = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(!record.runtime_started);
        mark_migration_started(&pair.a).unwrap();
        let record: MigrationRecord = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(record.runtime_started);
        assert_eq!(
            fs::read(pair.a.root().join("settings.v1")).unwrap(),
            app_before
        );
        assert_eq!(
            fs::read(pair.a.root().join("cross_screen.v1")).unwrap(),
            cross_before
        );
        assert!(WorkspaceConfiguration::prepare_legacy_migration(&pair.a).is_err());
        assert_eq!(
            WorkspaceLayout::decode(
                &fs::read(pair.a.root().join(LAYOUT_FILE)).unwrap(),
                &config
                    .connections
                    .members(pair.a.load_identity().unwrap().fingerprint().bytes()),
                &super::super::screen_edges(config.layout.positions())
            )
            .unwrap(),
            config.layout
        );
    }

    #[test]
    fn migration_collision_is_detected_before_writing_any_other_output() {
        let pair = TestPair::new();
        fs::write(pair.a.root().join(BEHAVIOR_FILE), b"user-owned").unwrap();
        assert!(WorkspaceConfiguration::prepare_legacy_migration(&pair.a).is_err());
        assert!(!pair.a.root().join(CONNECTIONS_FILE).exists());
        assert!(!pair.a.root().join(LAYOUT_FILE).exists());
        assert_eq!(
            fs::read(pair.a.root().join(BEHAVIOR_FILE)).unwrap(),
            b"user-owned"
        );
    }

    #[test]
    fn migration_resumes_matching_partial_outputs_but_rejects_postprepare_corruption() {
        let pair = TestPair::new();
        let config = WorkspaceConfiguration::from_legacy(&pair.a).unwrap();
        fs::write(
            pair.a.root().join(CONNECTIONS_FILE),
            serde_json::to_vec(&config.connections).unwrap(),
        )
        .unwrap();
        WorkspaceConfiguration::prepare_legacy_migration(&pair.a).unwrap();
        fs::write(pair.a.root().join(LAYOUT_FILE), b"truncated").unwrap();
        assert!(mark_migration_started(&pair.a).is_err());
        let record: MigrationRecord =
            serde_json::from_slice(&fs::read(pair.a.root().join(MIGRATION_FILE)).unwrap()).unwrap();
        assert!(!record.runtime_started);
    }

    #[test]
    fn atomic_layout_replacement_can_be_reread_and_connections_reject_duplicates() {
        let pair = TestPair::new();
        let mut config = WorkspaceConfiguration::prepare_legacy_migration(&pair.a).unwrap();
        let local = pair.a.load_identity().unwrap().fingerprint().bytes();
        let peer = config.connections.peers[0].peer;
        let members = config.connections.members(local);
        let edges = BTreeSet::from([edge(local, peer)]);
        let edited = config
            .layout
            .place(
                peer,
                config.layout.positions()[&local],
                local,
                &members,
                &edges,
            )
            .unwrap()
            .candidate;
        save_layout(&pair.a, &edited).unwrap();
        assert_eq!(
            WorkspaceConfiguration::load(&pair.a, &edges)
                .unwrap()
                .layout,
            edited
        );
        config
            .connections
            .peers
            .push(config.connections.peers[0].clone());
        assert!(config.connections.validate(&pair.a).is_err());
    }

    #[test]
    fn control_epoch_ranges_advance_durably_across_runtime_restarts() {
        let pair = TestPair::new();
        let first = reserve_control_epoch_range(&pair.a).unwrap();
        let second = reserve_control_epoch_range(&pair.a).unwrap();
        assert_eq!(
            first,
            ControlEpochRange {
                floor: CONTROL_EPOCH_BLOCK,
                limit: CONTROL_EPOCH_BLOCK * 2,
            }
        );
        assert_eq!(second.floor, first.limit);
        assert_eq!(second.limit, CONTROL_EPOCH_BLOCK * 3);

        let record: ControlEpochRecord =
            serde_json::from_slice(&fs::read(pair.a.root().join(CONTROL_EPOCH_FILE)).unwrap())
                .unwrap();
        assert_eq!(record.schema, 1);
        assert_eq!(record.next_floor, second.limit);
    }

    #[test]
    fn corrupt_control_epoch_state_is_not_silently_reset() {
        let pair = TestPair::new();
        fs::write(pair.a.root().join(CONTROL_EPOCH_FILE), b"truncated").unwrap();
        assert!(reserve_control_epoch_range(&pair.a).is_err());
        assert_eq!(
            fs::read(pair.a.root().join(CONTROL_EPOCH_FILE)).unwrap(),
            b"truncated"
        );
    }
}
