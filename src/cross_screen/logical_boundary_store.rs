use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::str::FromStr as _;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::logical_boundary::{
    BoundaryLayoutV3, BoundaryScreen, CompiledBoundaryLayout, LogicalBoundary,
};
use crate::device_store::DeviceStore;
use crate::security::CertificateFingerprint;

pub const LOGICAL_BOUNDARY_WORKSPACE_SCHEMA_VERSION: u16 = 3;
pub const LOGICAL_BOUNDARY_CORNER_GUARD_PIXELS: u32 = 32;

const BOUNDARY_WORKSPACE_FILE: &str = "cross_screen.boundary_workspace.v3.json";
const BOUNDARY_WORKSPACE_NEXT_FILE: &str = "cross_screen.boundary_workspace.v3.next";
const SCREEN_CATALOG_FILE: &str = "cross_screen.screen_catalog.v1.json";
const SCREEN_CATALOG_NEXT_FILE: &str = "cross_screen.screen_catalog.v1.next";
const BOUNDARY_WORKSPACE_FILE_LIMIT: u64 = 256 * 1024;
const SCREEN_CATALOG_FILE_LIMIT: u64 = 64 * 1024;
const SCREEN_CATALOG_SCHEMA_VERSION: u16 = 1;
const SCREEN_CATALOG_LIMIT: usize = 64;
const MAX_CORNER_GUARD_PIXELS: u32 = 512;
static SCREEN_CATALOG_UPDATE_LOCK: Mutex<()> = Mutex::new(());

/// The durable authoring state and the last configuration that passed the complete apply gate.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BoundaryWorkspaceV3 {
    pub schema_version: u16,
    pub storage_revision: u64,
    pub draft: BoundaryLayoutV3,
    pub applied: Option<AppliedBoundaryLayoutV3>,
}

/// Exact source data and compiler inputs for one immutable applied runtime snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AppliedBoundaryLayoutV3 {
    pub layout: BoundaryLayoutV3,
    pub corner_guard_pixels: u32,
    pub minimum_portal_pixels: u32,
}

/// Last screen dimensions received from authenticated peers, independent of layout membership.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SynchronizedScreenInfo {
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub physical_width_millimeters: u32,
    pub physical_height_millimeters: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SynchronizedScreenCatalog {
    #[serde(default = "screen_catalog_schema_version")]
    schema_version: u16,
    #[serde(default)]
    screens: HashMap<String, SynchronizedScreenInfo>,
}

impl Default for SynchronizedScreenCatalog {
    fn default() -> Self {
        Self {
            schema_version: SCREEN_CATALOG_SCHEMA_VERSION,
            screens: HashMap::new(),
        }
    }
}

const fn screen_catalog_schema_version() -> u16 {
    SCREEN_CATALOG_SCHEMA_VERSION
}

impl SynchronizedScreenInfo {
    /// Creates validated physical and pixel screen metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when a dimension is outside the logical-boundary limits.
    pub fn new(
        pixel_width: u32,
        pixel_height: u32,
        physical_width_millimeters: u32,
        physical_height_millimeters: u32,
    ) -> io::Result<Self> {
        super::PhysicalScreenSize::new(pixel_width, pixel_height)?;
        if physical_width_millimeters == 0
            || physical_height_millimeters == 0
            || physical_width_millimeters > 10_000
            || physical_height_millimeters > 10_000
        {
            return Err(invalid_data(
                "screen physical dimensions must be between 1 and 10000 millimeters",
            ));
        }
        Ok(Self {
            pixel_width,
            pixel_height,
            physical_width_millimeters,
            physical_height_millimeters,
        })
    }
}

impl SynchronizedScreenCatalog {
    /// Loads the durable peer screen cache, returning an empty catalog when none exists yet.
    ///
    /// # Errors
    ///
    /// Returns an error when the cache exists but is malformed, oversized, or inaccessible.
    pub fn load_or_default(store: &DeviceStore) -> io::Result<Self> {
        let path = screen_catalog_path(store);
        let file = match OpenOptions::new().read(true).open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error),
        };
        let metadata = file.metadata()?;
        if metadata.len() == 0 || metadata.len() > SCREEN_CATALOG_FILE_LIMIT {
            return Err(invalid_data(
                "synchronized screen catalog length is outside the allowed bounds",
            ));
        }
        let mut bytes = Vec::new();
        file.take(SCREEN_CATALOG_FILE_LIMIT + 1)
            .read_to_end(&mut bytes)?;
        let catalog = serde_json::from_slice::<Self>(&bytes).map_err(|error| {
            invalid_data(format!("invalid synchronized screen catalog: {error}"))
        })?;
        catalog.validate()?;
        Ok(catalog)
    }

    #[must_use]
    pub fn get(&self, fingerprint: CertificateFingerprint) -> Option<SynchronizedScreenInfo> {
        self.screens.get(&fingerprint.to_string()).copied()
    }

    /// Atomically merges one authenticated peer's latest screen metadata into the durable cache.
    ///
    /// This reloads while holding the process-wide catalog update lock so concurrent settings UI
    /// refreshes and background peer probes cannot overwrite one another with stale snapshots.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog cannot be loaded, validated, or durably updated.
    pub fn remember_latest(
        store: &DeviceStore,
        fingerprint: CertificateFingerprint,
        info: SynchronizedScreenInfo,
    ) -> io::Result<bool> {
        let _guard = SCREEN_CATALOG_UPDATE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut catalog = Self::load_or_default(store)?;
        catalog.remember(store, fingerprint, info)
    }

    /// Remembers the latest dimensions for one authenticated peer and writes only on change.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid metadata, a full catalog, or a durable-write failure.
    pub fn remember(
        &mut self,
        store: &DeviceStore,
        fingerprint: CertificateFingerprint,
        info: SynchronizedScreenInfo,
    ) -> io::Result<bool> {
        SynchronizedScreenInfo::new(
            info.pixel_width,
            info.pixel_height,
            info.physical_width_millimeters,
            info.physical_height_millimeters,
        )?;
        let key = fingerprint.to_string();
        if self.screens.get(&key) == Some(&info) {
            return Ok(false);
        }
        if !self.screens.contains_key(&key) && self.screens.len() >= SCREEN_CATALOG_LIMIT {
            return Err(invalid_data("synchronized screen catalog is full"));
        }
        self.screens.insert(key, info);
        self.validate()?;
        fs::create_dir_all(store.root())?;
        let bytes = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        if bytes.is_empty()
            || u64::try_from(bytes.len()).unwrap_or(u64::MAX) > SCREEN_CATALOG_FILE_LIMIT
        {
            return Err(invalid_data(
                "encoded synchronized screen catalog exceeds its size limit",
            ));
        }
        write_atomic(
            &screen_catalog_path(store),
            &screen_catalog_next_path(store),
            &bytes,
            ReplaceMode::ReplaceExisting,
        )?;
        Ok(true)
    }

    fn validate(&self) -> io::Result<()> {
        if self.schema_version != SCREEN_CATALOG_SCHEMA_VERSION {
            return Err(invalid_data(
                "unsupported synchronized screen catalog schema version",
            ));
        }
        if self.screens.len() > SCREEN_CATALOG_LIMIT {
            return Err(invalid_data(
                "synchronized screen catalog exceeds its limit",
            ));
        }
        for (fingerprint, info) in &self.screens {
            CertificateFingerprint::from_str(fingerprint)?;
            SynchronizedScreenInfo::new(
                info.pixel_width,
                info.pixel_height,
                info.physical_width_millimeters,
                info.physical_height_millimeters,
            )?;
        }
        Ok(())
    }
}

impl BoundaryWorkspaceV3 {
    /// Creates the first v3 workspace without replacing an existing file.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed authoring data, an existing workspace, oversized output,
    /// or a durable-write failure.
    pub fn initialize(
        store: &DeviceStore,
        mut initial_draft: BoundaryLayoutV3,
    ) -> io::Result<Self> {
        initial_draft.canonicalize_fingerprints()?;
        initial_draft.validate_schema()?;
        let workspace = Self {
            schema_version: LOGICAL_BOUNDARY_WORKSPACE_SCHEMA_VERSION,
            storage_revision: 0,
            draft: initial_draft,
            applied: None,
        };
        workspace.write_new(store)?;
        Ok(workspace)
    }

    /// Loads and validates the bounded v3 workspace file.
    ///
    /// # Errors
    ///
    /// Returns an error for missing, inaccessible, oversized, malformed, or inconsistent data.
    pub fn load(store: &DeviceStore) -> io::Result<Self> {
        let path = workspace_path(store);
        let file = OpenOptions::new().read(true).open(path)?;
        let metadata = file.metadata()?;
        if metadata.len() == 0 || metadata.len() > BOUNDARY_WORKSPACE_FILE_LIMIT {
            return Err(invalid_data(
                "logical boundary workspace file length is outside the allowed bounds",
            ));
        }
        let mut bytes = Vec::new();
        file.take(BOUNDARY_WORKSPACE_FILE_LIMIT + 1)
            .read_to_end(&mut bytes)?;
        Self::decode(&bytes)
    }

    /// Replaces only the editable draft and preserves the last applied snapshot.
    ///
    /// Both the workspace revision and the draft layout revision must match the current file.
    /// Mapping geometry changes must advance the affected boundary's mapping revision exactly
    /// once. The store advances the draft layout revision after validation.
    ///
    /// # Errors
    ///
    /// Returns an error for stale edits, reused mapping revisions, invalid geometry, corrupt
    /// existing data, oversized output, or a durable-write failure.
    pub fn replace_draft(
        store: &DeviceStore,
        expected_storage_revision: u64,
        mut proposed: BoundaryLayoutV3,
    ) -> io::Result<Self> {
        let current = Self::load(store)?;
        if current.storage_revision != expected_storage_revision
            || proposed.revision != current.draft.revision
        {
            return Err(stale_edit());
        }
        proposed.canonicalize_fingerprints()?;
        proposed.validate_schema()?;
        if proposed == current.draft {
            return Ok(current);
        }
        validate_draft_transition(&current.draft, &proposed)?;
        proposed.revision = proposed
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid_data("logical boundary layout revision is exhausted"))?;

        let replacement = Self {
            schema_version: LOGICAL_BOUNDARY_WORKSPACE_SCHEMA_VERSION,
            storage_revision: current
                .storage_revision
                .checked_add(1)
                .ok_or_else(|| invalid_data("logical boundary storage revision is exhausted"))?,
            draft: proposed,
            applied: current.applied,
        };
        replacement.write_replacement(store)?;
        Ok(replacement)
    }

    /// Replaces one authenticated peer's pair after the local user explicitly accepts it.
    ///
    /// Remote devices own their mapping-revision sequence, so this path does not require the
    /// incoming mapping revision to be the next local number. It still rejects any change to the
    /// local screen or to an unrelated peer's screens and boundaries.
    ///
    /// # Errors
    ///
    /// Returns an error for stale state, unrelated changes, invalid data, or a durable-write
    /// failure.
    pub fn replace_draft_from_authenticated_peer(
        store: &DeviceStore,
        expected_storage_revision: u64,
        peer: CertificateFingerprint,
        mut proposed: BoundaryLayoutV3,
    ) -> io::Result<Self> {
        let current = Self::load(store)?;
        if current.storage_revision != expected_storage_revision
            || proposed.revision != current.draft.revision
        {
            return Err(stale_edit());
        }
        proposed.canonicalize_fingerprints()?;
        proposed.validate_schema()?;
        if proposed == current.draft {
            return Ok(current);
        }
        validate_authenticated_peer_replacement(&current.draft, &proposed, &peer.to_string())?;
        proposed.revision = proposed
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid_data("logical boundary layout revision is exhausted"))?;
        let replacement = Self {
            schema_version: LOGICAL_BOUNDARY_WORKSPACE_SCHEMA_VERSION,
            storage_revision: current
                .storage_revision
                .checked_add(1)
                .ok_or_else(|| invalid_data("logical boundary storage revision is exhausted"))?,
            draft: proposed,
            applied: current.applied,
        };
        replacement.write_replacement(store)?;
        Ok(replacement)
    }

    /// Applies the current draft only when every referenced peer is online and compilation passes.
    ///
    /// The apply operation leaves the editable draft unchanged. It atomically publishes a copy of
    /// that draft together with the exact guard and minimum-length inputs used by the compiler.
    ///
    /// # Errors
    ///
    /// Returns an error for stale state, offline peers, unconfirmed mappings, conflicts, invalid
    /// compiler inputs, corrupt existing data, oversized output, or a durable-write failure.
    pub fn apply(
        store: &DeviceStore,
        expected_storage_revision: u64,
        online_peers: &HashSet<CertificateFingerprint>,
        corner_guard_pixels: u32,
        minimum_portal_pixels: u32,
    ) -> io::Result<(Self, CompiledBoundaryLayout)> {
        let current = Self::load(store)?;
        if current.storage_revision != expected_storage_revision {
            return Err(stale_edit());
        }
        validate_online_peers(&current.draft, online_peers)?;
        let compiled = current
            .draft
            .compile(corner_guard_pixels, minimum_portal_pixels)?;
        let applied = AppliedBoundaryLayoutV3 {
            layout: current.draft.clone(),
            corner_guard_pixels,
            minimum_portal_pixels,
        };
        applied.validate()?;
        let replacement = Self {
            schema_version: LOGICAL_BOUNDARY_WORKSPACE_SCHEMA_VERSION,
            storage_revision: current
                .storage_revision
                .checked_add(1)
                .ok_or_else(|| invalid_data("logical boundary storage revision is exhausted"))?,
            draft: current.draft,
            applied: Some(applied),
        };
        replacement.write_replacement(store)?;
        Ok((replacement, compiled))
    }

    /// Compiles the last successfully applied snapshot without consulting the editable draft.
    ///
    /// # Errors
    ///
    /// Returns an error when the persisted applied snapshot is internally inconsistent.
    pub fn compile_applied(&self) -> io::Result<Option<CompiledBoundaryLayout>> {
        self.applied
            .as_ref()
            .map(AppliedBoundaryLayoutV3::compile)
            .transpose()
    }

    /// Publishes only one peer's confirmed mapping while preserving unrelated applied mappings.
    ///
    /// This is the receiver-side half of an `Applied` synchronization message. The editable draft
    /// remains intact, including unrelated work that the local user has not applied yet.
    ///
    /// # Errors
    ///
    /// Returns an error for stale state, missing or unconfirmed pair data, an incompatible local
    /// screen change, compiler rejection, or a durable-write failure.
    pub fn apply_confirmed_pair(
        store: &DeviceStore,
        expected_storage_revision: u64,
        peer: CertificateFingerprint,
        corner_guard_pixels: u32,
        minimum_portal_pixels: u32,
    ) -> io::Result<(Self, CompiledBoundaryLayout)> {
        let current = Self::load(store)?;
        if current.storage_revision != expected_storage_revision {
            return Err(stale_edit());
        }
        let candidate = confirmed_pair_candidate(&current, &peer.to_string())?;
        let applied = AppliedBoundaryLayoutV3 {
            layout: candidate,
            corner_guard_pixels,
            minimum_portal_pixels,
        };
        applied.validate()?;
        let compiled = applied.compile()?;
        let replacement = Self {
            schema_version: LOGICAL_BOUNDARY_WORKSPACE_SCHEMA_VERSION,
            storage_revision: current
                .storage_revision
                .checked_add(1)
                .ok_or_else(|| invalid_data("logical boundary storage revision is exhausted"))?,
            draft: current.draft,
            applied: Some(applied),
        };
        replacement.write_replacement(store)?;
        Ok((replacement, compiled))
    }

    #[must_use]
    pub fn has_unapplied_changes(&self) -> bool {
        self.applied
            .as_ref()
            .is_none_or(|applied| !runtime_layouts_equivalent(&applied.layout, &self.draft))
    }

    fn decode(bytes: &[u8]) -> io::Result<Self> {
        let mut workspace =
            serde_json::from_slice::<Self>(bytes).map_err(|error| invalid_json(&error))?;
        workspace.draft.canonicalize_fingerprints()?;
        if let Some(applied) = &mut workspace.applied {
            applied.layout.canonicalize_fingerprints()?;
        }
        workspace.validate()?;
        Ok(workspace)
    }

    fn validate(&self) -> io::Result<()> {
        if self.schema_version != LOGICAL_BOUNDARY_WORKSPACE_SCHEMA_VERSION {
            return Err(invalid_data(
                "unsupported logical boundary workspace schema version",
            ));
        }
        if self.storage_revision == u64::MAX {
            return Err(invalid_data(
                "logical boundary storage revision uses the reserved maximum",
            ));
        }
        self.draft.validate_schema()?;
        if let Some(applied) = &self.applied {
            applied.validate()?;
            if applied.layout.local_fingerprint != self.draft.local_fingerprint {
                return Err(invalid_data(
                    "draft and applied layouts belong to different local devices",
                ));
            }
            if applied.layout.revision > self.draft.revision {
                return Err(invalid_data(
                    "applied layout revision exceeds the editable draft revision",
                ));
            }
        }
        Ok(())
    }

    fn write_new(&self, store: &DeviceStore) -> io::Result<()> {
        self.validate()?;
        fs::create_dir_all(store.root())?;
        let path = workspace_path(store);
        if path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "logical boundary workspace already exists",
            ));
        }
        let bytes = self.encode()?;
        write_atomic(
            &path,
            &workspace_next_path(store),
            &bytes,
            ReplaceMode::CreateNew,
        )
    }

    fn write_replacement(&self, store: &DeviceStore) -> io::Result<()> {
        self.validate()?;
        let bytes = self.encode()?;
        write_atomic(
            &workspace_path(store),
            &workspace_next_path(store),
            &bytes,
            ReplaceMode::ReplaceExisting,
        )
    }

    fn encode(&self) -> io::Result<Vec<u8>> {
        let bytes = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        if bytes.is_empty()
            || u64::try_from(bytes.len()).unwrap_or(u64::MAX) > BOUNDARY_WORKSPACE_FILE_LIMIT
        {
            return Err(invalid_data(
                "encoded logical boundary workspace exceeds its size limit",
            ));
        }
        Ok(bytes)
    }
}

impl AppliedBoundaryLayoutV3 {
    /// Rebuilds the immutable in-memory runtime snapshot from the exact applied inputs.
    ///
    /// # Errors
    ///
    /// Returns an error when the applied layout or compiler inputs are invalid.
    pub fn compile(&self) -> io::Result<CompiledBoundaryLayout> {
        self.validate_inputs()?;
        self.layout
            .compile(self.corner_guard_pixels, self.minimum_portal_pixels)
    }

    fn validate(&self) -> io::Result<()> {
        self.layout.validate_schema()?;
        self.compile().map(|_| ())
    }

    fn validate_inputs(&self) -> io::Result<()> {
        if self.corner_guard_pixels > MAX_CORNER_GUARD_PIXELS {
            return Err(invalid_data(
                "logical boundary corner guard exceeds 512 physical pixels",
            ));
        }
        if self.minimum_portal_pixels == 0 {
            return Err(invalid_data(
                "logical boundary minimum portal length must be positive",
            ));
        }
        Ok(())
    }
}

fn validate_online_peers(
    draft: &BoundaryLayoutV3,
    online_peers: &HashSet<CertificateFingerprint>,
) -> io::Result<()> {
    for boundary in &draft.boundaries {
        let peer = CertificateFingerprint::from_str(&boundary.peer_fingerprint)?;
        if !online_peers.contains(&peer) {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                format!(
                    "logical boundary {} cannot be applied while its peer is offline",
                    boundary.id
                ),
            ));
        }
    }
    Ok(())
}

fn validate_draft_transition(
    current: &BoundaryLayoutV3,
    proposed: &BoundaryLayoutV3,
) -> io::Result<()> {
    if current.local_fingerprint != proposed.local_fingerprint {
        return Err(invalid_data(
            "logical boundary workspace cannot change its local device",
        ));
    }
    let current_screens = screens_by_fingerprint(&current.screens);
    let proposed_screens = screens_by_fingerprint(&proposed.screens);
    let current_boundaries = current
        .boundaries
        .iter()
        .map(|boundary| (boundary.id.as_str(), boundary))
        .collect::<HashMap<_, _>>();

    for boundary in &proposed.boundaries {
        let Some(previous) = current_boundaries.get(boundary.id.as_str()).copied() else {
            continue;
        };
        let mapping_changed = boundary_mapping_changed(
            current,
            proposed,
            previous,
            boundary,
            &current_screens,
            &proposed_screens,
        );
        let expected_revision = if mapping_changed {
            previous
                .mapping_revision
                .checked_add(1)
                .ok_or_else(|| invalid_data("logical boundary mapping revision is exhausted"))?
        } else {
            previous.mapping_revision
        };
        if boundary.mapping_revision != expected_revision {
            return Err(invalid_data(if mapping_changed {
                "changed logical boundary mapping must advance its revision exactly once"
            } else {
                "unchanged logical boundary mapping cannot change its revision"
            }));
        }
    }
    Ok(())
}

fn validate_authenticated_peer_replacement(
    current: &BoundaryLayoutV3,
    proposed: &BoundaryLayoutV3,
    peer: &str,
) -> io::Result<()> {
    if current.local_fingerprint != proposed.local_fingerprint {
        return Err(invalid_data(
            "authenticated peer preview cannot change the local device",
        ));
    }
    let current_screens = screens_by_fingerprint(&current.screens);
    let proposed_screens = screens_by_fingerprint(&proposed.screens);
    for (fingerprint, screen) in &current_screens {
        if *fingerprint != peer && proposed_screens.get(fingerprint).copied() != Some(*screen) {
            return Err(invalid_data(
                "authenticated peer preview changed an unrelated screen",
            ));
        }
    }
    for (fingerprint, screen) in &proposed_screens {
        if *fingerprint != peer && current_screens.get(fingerprint).copied() != Some(*screen) {
            return Err(invalid_data(
                "authenticated peer preview added an unrelated screen",
            ));
        }
    }
    let unchanged_boundaries = |source: &BoundaryLayoutV3, other: &BoundaryLayoutV3| {
        source
            .boundaries
            .iter()
            .filter(|boundary| boundary.peer_fingerprint != peer)
            .all(|boundary| {
                other
                    .boundaries
                    .iter()
                    .any(|candidate| candidate == boundary)
            })
    };
    if !unchanged_boundaries(current, proposed)
        || !unchanged_boundaries(proposed, current)
        || proposed.boundaries.iter().any(|boundary| {
            boundary.peer_fingerprint != peer && !current.boundaries.contains(boundary)
        })
    {
        return Err(invalid_data(
            "authenticated peer preview changed an unrelated boundary",
        ));
    }
    Ok(())
}

fn runtime_layouts_equivalent(first: &BoundaryLayoutV3, second: &BoundaryLayoutV3) -> bool {
    if first.local_fingerprint != second.local_fingerprint
        || first.boundaries.len() != second.boundaries.len()
        || first.boundaries.iter().any(|boundary| {
            second
                .boundaries
                .iter()
                .find(|candidate| candidate.id == boundary.id)
                != Some(boundary)
        })
    {
        return false;
    }
    let first_screens = screens_by_fingerprint(&first.screens);
    let second_screens = screens_by_fingerprint(&second.screens);
    let relevant_screens = std::iter::once(first.local_fingerprint.as_str()).chain(
        first
            .boundaries
            .iter()
            .map(|boundary| boundary.peer_fingerprint.as_str()),
    );
    relevant_screens.into_iter().all(|fingerprint| {
        !screen_mapping_changed(
            first_screens.get(fingerprint).copied(),
            second_screens.get(fingerprint).copied(),
        )
    })
}

fn boundary_mapping_changed(
    current: &BoundaryLayoutV3,
    proposed: &BoundaryLayoutV3,
    previous: &LogicalBoundary,
    boundary: &LogicalBoundary,
    current_screens: &HashMap<&str, &BoundaryScreen>,
    proposed_screens: &HashMap<&str, &BoundaryScreen>,
) -> bool {
    if previous.peer_fingerprint != boundary.peer_fingerprint
        || previous.reversed != boundary.reversed
        || previous.source != boundary.source
        || previous.target != boundary.target
    {
        return true;
    }
    screen_mapping_changed(
        current_screens
            .get(current.local_fingerprint.as_str())
            .copied(),
        proposed_screens
            .get(proposed.local_fingerprint.as_str())
            .copied(),
    ) || screen_mapping_changed(
        current_screens
            .get(previous.peer_fingerprint.as_str())
            .copied(),
        proposed_screens
            .get(boundary.peer_fingerprint.as_str())
            .copied(),
    )
}

fn screens_by_fingerprint(screens: &[BoundaryScreen]) -> HashMap<&str, &BoundaryScreen> {
    screens
        .iter()
        .map(|screen| (screen.device_fingerprint.as_str(), screen))
        .collect()
}

fn confirmed_pair_candidate(
    current: &BoundaryWorkspaceV3,
    peer: &str,
) -> io::Result<BoundaryLayoutV3> {
    let local = current.draft.local_fingerprint.as_str();
    let draft_screens = screens_by_fingerprint(&current.draft.screens);
    let draft_local = draft_screens
        .get(local)
        .copied()
        .ok_or_else(|| invalid_data("logical boundary draft is missing the local screen"))?;
    let draft_peer = draft_screens
        .get(peer)
        .copied()
        .ok_or_else(|| invalid_data("logical boundary draft is missing the applied peer screen"))?;
    let pair_boundaries = current
        .draft
        .boundaries
        .iter()
        .filter(|boundary| boundary.peer_fingerprint == peer)
        .cloned()
        .collect::<Vec<_>>();
    if pair_boundaries.iter().any(|boundary| {
        boundary.local_confirmed_revision != boundary.mapping_revision
            || boundary.peer_confirmed_revision != boundary.mapping_revision
    }) {
        return Err(invalid_data(
            "remote applied mapping is not confirmed by both devices",
        ));
    }

    let mut candidate = current.applied.as_ref().map_or_else(
        || BoundaryLayoutV3 {
            schema_version: current.draft.schema_version,
            revision: current.draft.revision,
            local_fingerprint: current.draft.local_fingerprint.clone(),
            screens: vec![draft_local.clone(), draft_peer.clone()],
            boundaries: pair_boundaries.clone(),
        },
        |applied| applied.layout.clone(),
    );
    let Some(previously_applied) = current.applied.as_ref() else {
        candidate.validate_schema()?;
        return Ok(candidate);
    };
    let applied_local = screens_by_fingerprint(&previously_applied.layout.screens)
        .get(local)
        .copied()
        .ok_or_else(|| invalid_data("applied layout is missing the local screen"))?;
    let has_other_boundaries = candidate
        .boundaries
        .iter()
        .any(|boundary| boundary.peer_fingerprint != peer);
    let local_mapping_changed = screen_mapping_changed(Some(applied_local), Some(draft_local));
    if has_other_boundaries && local_mapping_changed {
        return Err(invalid_data(
            "a peer-applied mapping cannot change the local screen used by other mappings",
        ));
    }

    let offset_x =
        i64::from(applied_local.canvas_x_millimeters) - i64::from(draft_local.canvas_x_millimeters);
    let offset_y =
        i64::from(applied_local.canvas_y_millimeters) - i64::from(draft_local.canvas_y_millimeters);
    let mut peer_screen = draft_peer.clone();
    peer_screen.canvas_x_millimeters = shifted_canvas_coordinate(
        peer_screen.canvas_x_millimeters,
        offset_x,
        "peer-applied screen x position",
    )?;
    peer_screen.canvas_y_millimeters = shifted_canvas_coordinate(
        peer_screen.canvas_y_millimeters,
        offset_y,
        "peer-applied screen y position",
    )?;
    if local_mapping_changed {
        candidate
            .screens
            .retain(|screen| screen.device_fingerprint != current.draft.local_fingerprint);
        candidate.screens.push(draft_local.clone());
        peer_screen = draft_peer.clone();
    }
    candidate
        .screens
        .retain(|screen| screen.device_fingerprint != peer);
    candidate.screens.push(peer_screen);
    candidate
        .boundaries
        .retain(|boundary| boundary.peer_fingerprint != peer);
    candidate.boundaries.extend(pair_boundaries);
    candidate.revision = current.draft.revision;
    candidate.validate_schema()?;
    Ok(candidate)
}

fn screen_mapping_changed(
    current: Option<&BoundaryScreen>,
    proposed: Option<&BoundaryScreen>,
) -> bool {
    match (current, proposed) {
        (Some(current), Some(proposed)) => {
            current.pixel_width != proposed.pixel_width
                || current.pixel_height != proposed.pixel_height
                || current.physical_width_millimeters != proposed.physical_width_millimeters
                || current.physical_height_millimeters != proposed.physical_height_millimeters
                || current.rotation_quarter_turns != proposed.rotation_quarter_turns
        }
        _ => true,
    }
}

fn shifted_canvas_coordinate(value: i32, offset: i64, label: &str) -> io::Result<i32> {
    i32::try_from(i64::from(value).saturating_add(offset))
        .map_err(|_| invalid_data(format!("{label} is outside the supported range")))
}

#[derive(Clone, Copy)]
enum ReplaceMode {
    CreateNew,
    ReplaceExisting,
}

fn write_atomic(path: &Path, temporary: &Path, bytes: &[u8], mode: ReplaceMode) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    let result = replace_file(temporary, path, mode);
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn replace_file(from: &Path, to: &Path, mode: ReplaceMode) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    let from = from
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let to = to
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let flags = match mode {
        ReplaceMode::CreateNew => MOVEFILE_WRITE_THROUGH,
        ReplaceMode::ReplaceExisting => MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
    };
    unsafe {
        MoveFileExW(
            windows_core::PCWSTR(from.as_ptr()),
            windows_core::PCWSTR(to.as_ptr()),
            flags,
        )
    }
    .map_err(io::Error::other)
}

fn workspace_path(store: &DeviceStore) -> PathBuf {
    store.root().join(BOUNDARY_WORKSPACE_FILE)
}

fn workspace_next_path(store: &DeviceStore) -> PathBuf {
    store.root().join(BOUNDARY_WORKSPACE_NEXT_FILE)
}

fn screen_catalog_path(store: &DeviceStore) -> PathBuf {
    store.root().join(SCREEN_CATALOG_FILE)
}

fn screen_catalog_next_path(store: &DeviceStore) -> PathBuf {
    store.root().join(SCREEN_CATALOG_NEXT_FILE)
}

fn stale_edit() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "logical boundary edit is based on a stale workspace revision",
    )
}

fn invalid_json(error: &serde_json::Error) -> io::Error {
    invalid_data(format!("invalid logical boundary workspace JSON: {error}"))
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::cross_screen::{
        BoundaryArcSegment, BoundaryPortal, LOGICAL_BOUNDARY_SCHEMA_VERSION, LogicalBoundary,
        NORMALIZED_PORTAL_LENGTH, ScreenBoundary,
    };

    static DIRECTORY_SERIAL: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let serial = DIRECTORY_SERIAL.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "pierdesk-boundary-workspace-{}-{serial}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fingerprint(byte: u8) -> CertificateFingerprint {
        CertificateFingerprint::from_bytes([byte; 32])
    }

    fn screen(byte: u8) -> BoundaryScreen {
        BoundaryScreen {
            device_fingerprint: fingerprint(byte).to_string(),
            pixel_width: 2_560,
            pixel_height: 1_440,
            physical_width_millimeters: 600,
            physical_height_millimeters: 340,
            rotation_quarter_turns: 0,
            canvas_x_millimeters: i32::from(byte) * 100,
            canvas_y_millimeters: 0,
        }
    }

    fn portal(edge: ScreenBoundary, start: u32, end: u32) -> BoundaryPortal {
        BoundaryPortal {
            segments: vec![BoundaryArcSegment { edge, start, end }],
        }
    }

    fn boundary() -> LogicalBoundary {
        LogicalBoundary {
            id: "a-to-b".to_owned(),
            peer_fingerprint: fingerprint(2).to_string(),
            mapping_revision: 4,
            local_confirmed_revision: 4,
            peer_confirmed_revision: 4,
            reversed: true,
            source: portal(ScreenBoundary::Right, 8_000, 45_000),
            target: portal(ScreenBoundary::Left, 12_000, 52_000),
        }
    }

    fn layout() -> BoundaryLayoutV3 {
        BoundaryLayoutV3 {
            schema_version: LOGICAL_BOUNDARY_SCHEMA_VERSION,
            revision: 0,
            local_fingerprint: fingerprint(1).to_string(),
            screens: vec![screen(1), screen(2)],
            boundaries: vec![boundary()],
        }
    }

    fn online() -> HashSet<CertificateFingerprint> {
        HashSet::from([fingerprint(2)])
    }

    #[test]
    fn initialization_and_draft_replacement_are_durable_and_stale_safe() {
        let directory = TestDirectory::new();
        let store = DeviceStore::new(&directory.0);
        let initial = BoundaryWorkspaceV3::initialize(&store, layout()).unwrap();
        assert_eq!(initial.storage_revision, 0);
        assert!(!workspace_next_path(&store).exists());
        assert_eq!(BoundaryWorkspaceV3::load(&store).unwrap(), initial);
        assert_eq!(
            BoundaryWorkspaceV3::initialize(&store, layout())
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );

        let mut edited = initial.draft.clone();
        edited.boundaries[0].source = portal(ScreenBoundary::Top, 18_000, 40_000);
        edited.boundaries[0].mapping_revision += 1;
        edited.boundaries[0].local_confirmed_revision += 1;
        let saved = BoundaryWorkspaceV3::replace_draft(&store, 0, edited.clone()).unwrap();
        assert_eq!(saved.storage_revision, 1);
        assert_eq!(saved.draft.revision, 1);
        assert_eq!(
            saved.draft.boundaries[0].target,
            initial.draft.boundaries[0].target
        );
        assert!(!saved.draft.boundaries[0].is_confirmed());

        assert!(BoundaryWorkspaceV3::replace_draft(&store, 0, edited).is_err());
        assert_eq!(BoundaryWorkspaceV3::load(&store).unwrap(), saved);
    }

    #[test]
    fn mapping_changes_require_exactly_one_new_mapping_revision() {
        let directory = TestDirectory::new();
        let store = DeviceStore::new(&directory.0);
        let initial = BoundaryWorkspaceV3::initialize(&store, layout()).unwrap();
        let mut changed = initial.draft.clone();
        changed.boundaries[0].target = portal(ScreenBoundary::Bottom, 10_000, 30_000);
        assert!(BoundaryWorkspaceV3::replace_draft(&store, 0, changed.clone()).is_err());

        changed.boundaries[0].mapping_revision += 2;
        assert!(BoundaryWorkspaceV3::replace_draft(&store, 0, changed).is_err());

        let mut confirmation_only = initial.draft;
        confirmation_only.boundaries[0].peer_confirmed_revision = 3;
        BoundaryWorkspaceV3::replace_draft(&store, 0, confirmation_only).unwrap();
    }

    #[test]
    fn explicit_authenticated_peer_accepts_its_revision_without_local_rebasing() {
        let directory = TestDirectory::new();
        let store = DeviceStore::new(&directory.0);
        let initial = BoundaryWorkspaceV3::initialize(&store, layout()).unwrap();
        let mut accepted = initial.draft;
        accepted.boundaries[0].target = portal(ScreenBoundary::Bottom, 4_000, 28_000);
        accepted.boundaries[0].mapping_revision = 19;
        accepted.boundaries[0].local_confirmed_revision = 19;
        accepted.boundaries[0].peer_confirmed_revision = 19;

        let saved = BoundaryWorkspaceV3::replace_draft_from_authenticated_peer(
            &store,
            0,
            fingerprint(2),
            accepted,
        )
        .unwrap();
        assert_eq!(saved.storage_revision, 1);
        assert_eq!(saved.draft.revision, 1);
        assert_eq!(saved.draft.boundaries[0].mapping_revision, 19);
        assert!(saved.draft.boundaries[0].is_confirmed());
    }

    #[test]
    fn authenticated_peer_replacement_rejects_local_or_unrelated_changes() {
        let directory = TestDirectory::new();
        let store = DeviceStore::new(&directory.0);
        let mut initial = layout();
        initial.screens.push(screen(3));
        initial.boundaries.push(LogicalBoundary {
            id: "a-to-c".to_owned(),
            peer_fingerprint: fingerprint(3).to_string(),
            mapping_revision: 2,
            local_confirmed_revision: 2,
            peer_confirmed_revision: 2,
            reversed: false,
            source: portal(ScreenBoundary::Bottom, 2_000, 18_000),
            target: portal(ScreenBoundary::Top, 3_000, 19_000),
        });
        BoundaryWorkspaceV3::initialize(&store, initial.clone()).unwrap();

        let mut local_changed = initial.clone();
        local_changed.screens[0].physical_width_millimeters += 1;
        assert!(
            BoundaryWorkspaceV3::replace_draft_from_authenticated_peer(
                &store,
                0,
                fingerprint(2),
                local_changed,
            )
            .is_err()
        );

        let mut unrelated_changed = initial;
        unrelated_changed.boundaries[1].mapping_revision += 1;
        assert!(
            BoundaryWorkspaceV3::replace_draft_from_authenticated_peer(
                &store,
                0,
                fingerprint(2),
                unrelated_changed,
            )
            .is_err()
        );
    }

    #[test]
    fn synchronized_screen_catalog_survives_layout_removal_and_restart() {
        let directory = TestDirectory::new();
        let store = DeviceStore::new(&directory.0);
        let info = SynchronizedScreenInfo::new(2_240, 1_400, 302, 188).unwrap();
        let mut catalog = SynchronizedScreenCatalog::load_or_default(&store).unwrap();
        assert!(catalog.remember(&store, fingerprint(2), info).unwrap());
        assert!(!catalog.remember(&store, fingerprint(2), info).unwrap());

        let loaded = SynchronizedScreenCatalog::load_or_default(&store).unwrap();
        assert_eq!(loaded.get(fingerprint(2)), Some(info));
        assert!(!screen_catalog_next_path(&store).exists());
    }

    #[test]
    fn concurrent_screen_updates_merge_without_losing_another_peer() {
        let directory = TestDirectory::new();
        let store = DeviceStore::new(&directory.0);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let workers = [
            (
                fingerprint(2),
                SynchronizedScreenInfo::new(2_240, 1_400, 302, 188).unwrap(),
            ),
            (
                fingerprint(3),
                SynchronizedScreenInfo::new(2_560, 1_440, 344, 194).unwrap(),
            ),
        ]
        .map(|(fingerprint, info)| {
            let store = store.clone();
            let barrier = std::sync::Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                SynchronizedScreenCatalog::remember_latest(&store, fingerprint, info).unwrap();
            })
        });
        barrier.wait();
        for worker in workers {
            worker.join().unwrap();
        }

        let catalog = SynchronizedScreenCatalog::load_or_default(&store).unwrap();
        assert_eq!(
            catalog.get(fingerprint(2)),
            Some(SynchronizedScreenInfo::new(2_240, 1_400, 302, 188).unwrap())
        );
        assert_eq!(
            catalog.get(fingerprint(3)),
            Some(SynchronizedScreenInfo::new(2_560, 1_440, 344, 194).unwrap())
        );
    }

    #[test]
    fn screen_mapping_changes_also_require_a_new_mapping_revision() {
        let directory = TestDirectory::new();
        let store = DeviceStore::new(&directory.0);
        let initial = BoundaryWorkspaceV3::initialize(&store, layout()).unwrap();
        let mut changed = initial.draft;
        changed.screens[1].physical_width_millimeters += 1;
        assert!(BoundaryWorkspaceV3::replace_draft(&store, 0, changed.clone()).is_err());
        changed.boundaries[0].mapping_revision += 1;
        BoundaryWorkspaceV3::replace_draft(&store, 0, changed).unwrap();
    }

    #[test]
    fn apply_is_offline_and_confirmation_gated_without_touching_the_old_snapshot() {
        let directory = TestDirectory::new();
        let store = DeviceStore::new(&directory.0);
        let initial = BoundaryWorkspaceV3::initialize(&store, layout()).unwrap();
        assert_eq!(
            BoundaryWorkspaceV3::apply(&store, 0, &HashSet::new(), 32, 64)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotConnected
        );
        assert_eq!(BoundaryWorkspaceV3::load(&store).unwrap(), initial);

        let mut pending = initial.draft;
        pending.boundaries[0].peer_confirmed_revision = 3;
        let pending = BoundaryWorkspaceV3::replace_draft(&store, 0, pending).unwrap();
        assert!(BoundaryWorkspaceV3::apply(&store, 1, &online(), 32, 64).is_err());
        assert_eq!(BoundaryWorkspaceV3::load(&store).unwrap(), pending);
    }

    #[test]
    fn successful_apply_publishes_an_independent_compilable_snapshot() {
        let directory = TestDirectory::new();
        let store = DeviceStore::new(&directory.0);
        BoundaryWorkspaceV3::initialize(&store, layout()).unwrap();
        let (applied, compiled) = BoundaryWorkspaceV3::apply(&store, 0, &online(), 32, 64).unwrap();
        assert_eq!(applied.storage_revision, 1);
        assert!(!applied.has_unapplied_changes());
        assert_eq!(compiled.boundaries().len(), 1);
        assert_eq!(
            applied.compile_applied().unwrap().unwrap().revision(),
            applied.draft.revision
        );

        let mut edited = applied.draft.clone();
        edited.boundaries[0].source =
            portal(ScreenBoundary::Right, 0, NORMALIZED_PORTAL_LENGTH / 2);
        edited.boundaries[0].mapping_revision += 1;
        edited.boundaries[0].local_confirmed_revision += 1;
        let pending = BoundaryWorkspaceV3::replace_draft(&store, 1, edited).unwrap();
        assert!(pending.has_unapplied_changes());
        assert_eq!(
            pending.compile_applied().unwrap().unwrap().revision(),
            applied.draft.revision
        );
    }

    #[test]
    fn peer_apply_updates_only_that_pair_and_preserves_unrelated_draft_work() {
        let directory = TestDirectory::new();
        let store = DeviceStore::new(&directory.0);
        let mut initial = layout();
        initial.screens.push(screen(3));
        initial.boundaries.push(LogicalBoundary {
            id: "a-to-c".to_owned(),
            peer_fingerprint: fingerprint(3).to_string(),
            mapping_revision: 4,
            local_confirmed_revision: 4,
            peer_confirmed_revision: 4,
            reversed: false,
            source: portal(ScreenBoundary::Bottom, 8_000, 24_000),
            target: portal(ScreenBoundary::Top, 12_000, 28_000),
        });
        BoundaryWorkspaceV3::initialize(&store, initial).unwrap();
        let online = HashSet::from([fingerprint(2), fingerprint(3)]);
        let (applied, _) = BoundaryWorkspaceV3::apply(&store, 0, &online, 32, 64).unwrap();
        let original_other = applied.draft.boundaries[1].clone();

        let mut edited = applied.draft.clone();
        edited.boundaries[0].source = portal(ScreenBoundary::Top, 4_000, 20_000);
        edited.boundaries[0].mapping_revision += 1;
        edited.boundaries[0].local_confirmed_revision += 1;
        edited.boundaries[0].peer_confirmed_revision += 1;
        edited.boundaries[1].source = portal(ScreenBoundary::Bottom, 30_000, 46_000);
        edited.boundaries[1].mapping_revision += 1;
        edited.boundaries[1].local_confirmed_revision += 1;
        let pending = BoundaryWorkspaceV3::replace_draft(&store, 1, edited).unwrap();

        let (saved, compiled) = BoundaryWorkspaceV3::apply_confirmed_pair(
            &store,
            pending.storage_revision,
            fingerprint(2),
            32,
            64,
        )
        .unwrap();
        let applied_layout = &saved.applied.as_ref().unwrap().layout;
        assert_eq!(
            applied_layout
                .boundaries
                .iter()
                .find(|boundary| boundary.peer_fingerprint == fingerprint(2).to_string())
                .unwrap()
                .mapping_revision,
            5
        );
        assert_eq!(
            applied_layout
                .boundaries
                .iter()
                .find(|boundary| boundary.peer_fingerprint == fingerprint(3).to_string())
                .unwrap(),
            &original_other
        );
        assert_eq!(saved.draft.boundaries[1].mapping_revision, 5);
        assert!(saved.has_unapplied_changes());
        assert_eq!(compiled.boundaries().len(), 2);
    }

    #[test]
    fn peer_apply_can_publish_the_first_confirmed_pair_without_an_online_snapshot() {
        let directory = TestDirectory::new();
        let store = DeviceStore::new(&directory.0);
        let initial = BoundaryWorkspaceV3::initialize(&store, layout()).unwrap();
        let (saved, compiled) = BoundaryWorkspaceV3::apply_confirmed_pair(
            &store,
            initial.storage_revision,
            fingerprint(2),
            32,
            64,
        )
        .unwrap();
        assert_eq!(saved.applied.as_ref().unwrap().layout, saved.draft);
        assert!(!saved.has_unapplied_changes());
        assert_eq!(compiled.boundaries().len(), 1);
    }

    #[test]
    fn canvas_only_edits_are_saved_without_marking_runtime_boundaries_pending() {
        let directory = TestDirectory::new();
        let store = DeviceStore::new(&directory.0);
        BoundaryWorkspaceV3::initialize(&store, layout()).unwrap();
        let (applied, _) = BoundaryWorkspaceV3::apply(&store, 0, &online(), 32, 64).unwrap();

        let mut moved = applied.draft.clone();
        moved.screens[0].canvas_x_millimeters += 50;
        moved.screens.push(screen(3));
        let moved = BoundaryWorkspaceV3::replace_draft(&store, 1, moved).unwrap();
        assert_eq!(moved.storage_revision, 2);
        assert_eq!(moved.draft.revision, 1);
        assert!(!moved.has_unapplied_changes());
        assert_eq!(
            moved
                .compile_applied()
                .unwrap()
                .unwrap()
                .boundary("a-to-b")
                .unwrap()
                .mapping_revision(),
            4
        );

        let unchanged = BoundaryWorkspaceV3::replace_draft(&store, 2, moved.draft.clone()).unwrap();
        assert_eq!(unchanged, moved);
    }

    #[test]
    fn moved_exit_keeps_the_applied_entry_until_the_peer_confirms() {
        let directory = TestDirectory::new();
        let store = DeviceStore::new(&directory.0);
        BoundaryWorkspaceV3::initialize(&store, layout()).unwrap();
        let (applied, _) = BoundaryWorkspaceV3::apply(&store, 0, &online(), 32, 64).unwrap();
        let original_target = applied.draft.boundaries[0].target.clone();

        let mut moved = applied.draft.clone();
        moved.boundaries[0].source = portal(ScreenBoundary::Top, 20_000, 48_000);
        moved.boundaries[0].mapping_revision += 1;
        moved.boundaries[0].local_confirmed_revision += 1;
        let pending = BoundaryWorkspaceV3::replace_draft(&store, 1, moved).unwrap();
        assert_eq!(pending.draft.boundaries[0].target, original_target);
        assert!(!pending.draft.boundaries[0].is_confirmed());
        assert_eq!(
            pending
                .compile_applied()
                .unwrap()
                .unwrap()
                .boundary("a-to-b")
                .unwrap()
                .mapping_revision(),
            4
        );
        assert!(BoundaryWorkspaceV3::apply(&store, 2, &online(), 32, 64).is_err());

        let mut confirmed = pending.draft;
        confirmed.boundaries[0].peer_confirmed_revision = confirmed.boundaries[0].mapping_revision;
        let confirmed = BoundaryWorkspaceV3::replace_draft(&store, 2, confirmed).unwrap();
        let (newly_applied, compiled) =
            BoundaryWorkspaceV3::apply(&store, 3, &online(), 32, 64).unwrap();
        assert_eq!(newly_applied.draft.boundaries[0].target, original_target);
        assert!(!newly_applied.has_unapplied_changes());
        assert_eq!(compiled.boundary("a-to-b").unwrap().mapping_revision(), 5);
        assert_eq!(newly_applied.draft, confirmed.draft);
    }

    #[test]
    fn corrupt_unknown_and_oversized_workspaces_fail_closed() {
        let directory = TestDirectory::new();
        let store = DeviceStore::new(&directory.0);
        let path = workspace_path(&store);
        fs::write(&path, b"not json").unwrap();
        assert!(BoundaryWorkspaceV3::load(&store).is_err());
        fs::write(
            &path,
            br#"{"schemaVersion":3,"storageRevision":0,"draft":{},"applied":null,"unknown":true}"#,
        )
        .unwrap();
        assert!(BoundaryWorkspaceV3::load(&store).is_err());
        fs::write(
            &path,
            vec![b'x'; usize::try_from(BOUNDARY_WORKSPACE_FILE_LIMIT + 1).unwrap()],
        )
        .unwrap();
        assert!(BoundaryWorkspaceV3::load(&store).is_err());
    }
}
