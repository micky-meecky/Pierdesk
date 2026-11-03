use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::path::Path;
use std::str::FromStr as _;

use serde::{Deserialize, Serialize};

use super::{PhysicalScreenSize, PointerSample, ScreenBoundary};
use crate::device_store::DeviceStore;
use crate::security::CertificateFingerprint;

pub const BOUNDARY_MAP_SCHEMA_VERSION: u16 = 2;
pub const NORMALIZED_EDGE_LENGTH: u32 = 65_536;
pub const MAX_BOUNDARY_PEERS: usize = 5;
pub const MAX_BOUNDARY_SEGMENTS: usize = 32;
pub const DEFAULT_MINIMUM_SEGMENT_PIXELS: u32 = 64;

const BOUNDARY_MAP_FILE: &str = "cross_screen.boundaries.v2.json";
const BOUNDARY_MAP_NEXT_FILE: &str = "cross_screen.boundaries.v2.next";
const BOUNDARY_MAP_FILE_LIMIT: u64 = 64 * 1024;
const IDENTIFIER_BYTE_LIMIT: usize = 64;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundaryCorner {
    TopLeft,
    TopRight,
    BottomRight,
    BottomLeft,
}

impl BoundaryCorner {
    const fn expected_edges(self) -> [ScreenBoundary; 2] {
        match self {
            Self::TopLeft => [ScreenBoundary::Top, ScreenBoundary::Left],
            Self::TopRight => [ScreenBoundary::Top, ScreenBoundary::Right],
            Self::BottomRight => [ScreenBoundary::Bottom, ScreenBoundary::Right],
            Self::BottomLeft => [ScreenBoundary::Bottom, ScreenBoundary::Left],
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BoundarySegment {
    pub id: String,
    pub peer_fingerprint: String,
    pub edge: ScreenBoundary,
    pub start: u32,
    pub end: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corner: Option<BoundaryCorner>,
}

impl BoundarySegment {
    #[must_use]
    pub const fn span(&self) -> u32 {
        self.end - self.start
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BoundaryMapV2 {
    pub schema_version: u16,
    pub revision: u64,
    pub segments: Vec<BoundarySegment>,
}

impl Default for BoundaryMapV2 {
    fn default() -> Self {
        Self {
            schema_version: BOUNDARY_MAP_SCHEMA_VERSION,
            revision: 0,
            segments: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BoundaryMapConstraints {
    pub screen: PhysicalScreenSize,
    pub corner_guard_enabled: bool,
    pub corner_guard_pixels: u32,
    pub minimum_segment_pixels: u32,
}

impl BoundaryMapConstraints {
    /// Validates the screen-dependent editing constraints.
    ///
    /// # Errors
    ///
    /// Returns an error when the guard consumes an edge or the minimum segment cannot fit.
    pub fn validate(self) -> io::Result<()> {
        if self.minimum_segment_pixels == 0 {
            return Err(invalid_data(
                "minimum boundary segment length must be positive",
            ));
        }
        for edge in all_edges() {
            let extent = edge_extent(edge, self.screen);
            let guard = if self.corner_guard_enabled {
                self.corner_guard_pixels
            } else {
                0
            };
            if guard.saturating_mul(2) >= extent {
                return Err(invalid_data("corner guard consumes a complete screen edge"));
            }
            if self.minimum_segment_pixels > extent - guard.saturating_mul(2) {
                return Err(invalid_data(
                    "minimum boundary segment length cannot fit the usable edge",
                ));
            }
        }
        Ok(())
    }

    fn normalized_safe_range(self, edge: ScreenBoundary) -> io::Result<(u32, u32)> {
        self.validate()?;
        let guard = if self.corner_guard_enabled {
            self.corner_guard_pixels
        } else {
            0
        };
        let extent = edge_extent(edge, self.screen);
        let start = physical_boundary_to_normalized(guard, extent)?;
        let end = u32::try_from(
            u64::from(extent - guard) * u64::from(NORMALIZED_EDGE_LENGTH) / u64::from(extent),
        )
        .expect("normalized safe boundary fits in u32");
        Ok((start, end))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundaryHit {
    pub segment_id: String,
    pub peer_fingerprint: String,
    pub edge: ScreenBoundary,
    pub normalized_offset: u32,
    pub group_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PointerMotion {
    pub from_x: i64,
    pub from_y: i64,
    pub to_x: i64,
    pub to_y: i64,
}

/// Immutable, screen-clipped boundary routes used by the input hot path.
///
/// Construction performs all schema, fingerprint, guard and minimum-length validation once.
/// Subsequent hit tests therefore never parse configuration or touch the filesystem.
#[derive(Clone, Debug)]
pub struct PreparedBoundaryMap {
    map: BoundaryMapV2,
    screen: PhysicalScreenSize,
}

impl PreparedBoundaryMap {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.segments.is_empty()
    }

    #[must_use]
    pub fn segment_count(&self) -> usize {
        self.map.segments.len()
    }

    /// Finds an assigned route for an already-clamped Windows pointer sample.
    ///
    /// A point at a geometric corner fails closed unless every matching segment belongs to the
    /// same peer and logical cross-corner group.
    ///
    /// # Errors
    ///
    /// Returns an error when the pointer is outside the prepared physical screen.
    pub fn hit_test_sample(&self, sample: PointerSample) -> io::Result<Option<BoundaryHit>> {
        validate_pointer_sample(sample, self.screen)?;
        let right = self.screen.width - 1;
        let bottom = self.screen.height - 1;
        let mut hits = Vec::with_capacity(2);
        if sample.position.x == 0 && sample.delta_x < 0 {
            self.push_hit(&mut hits, ScreenBoundary::Left, sample.position.y)?;
        }
        if sample.position.x == right && sample.delta_x > 0 {
            self.push_hit(&mut hits, ScreenBoundary::Right, sample.position.y)?;
        }
        if sample.position.y == 0 && sample.delta_y < 0 {
            self.push_hit(&mut hits, ScreenBoundary::Top, sample.position.x)?;
        }
        if sample.position.y == bottom && sample.delta_y > 0 {
            self.push_hit(&mut hits, ScreenBoundary::Bottom, sample.position.x)?;
        }
        resolve_hits(&hits)
    }

    /// Finds the first assigned route crossed by one fast raw-input projection.
    ///
    /// # Errors
    ///
    /// Returns an error when the motion starts outside the prepared physical screen.
    pub fn hit_test_motion(&self, motion: PointerMotion) -> io::Result<Option<BoundaryHit>> {
        validate_motion_start(motion, self.screen)?;
        let mut hits = Vec::with_capacity(2);
        for candidate in first_crossings(motion, self.screen) {
            self.push_hit(&mut hits, candidate.edge, candidate.offset)?;
        }
        resolve_hits(&hits)
    }

    fn push_hit(
        &self,
        hits: &mut Vec<BoundaryHit>,
        edge: ScreenBoundary,
        offset: u32,
    ) -> io::Result<()> {
        if let Some(hit) = hit_test_validated(&self.map, edge, offset, self.screen)? {
            hits.push(hit);
        }
        Ok(())
    }
}

impl BoundaryMapV2 {
    /// Builds one legacy full-edge map for a paired peer.
    ///
    /// This is used only when upgrading an installation that has never persisted a V2 map.
    ///
    /// # Errors
    ///
    /// Returns an error when the screen constraints cannot represent a usable legacy edge.
    pub fn legacy_full_edge(
        peer: CertificateFingerprint,
        edge: ScreenBoundary,
        constraints: BoundaryMapConstraints,
    ) -> io::Result<Self> {
        let (start, end) = constraints.normalized_safe_range(edge)?;
        let map = Self {
            schema_version: BOUNDARY_MAP_SCHEMA_VERSION,
            revision: 0,
            segments: vec![BoundarySegment {
                id: "legacy-full-edge".to_owned(),
                peer_fingerprint: peer.to_string(),
                edge,
                start,
                end,
                group_id: None,
                corner: None,
            }],
        };
        map.validate_for_screen(constraints)?;
        Ok(map)
    }

    /// Validates, clips and filters this map for one currently connected peer.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid stored geometry or unusable physical-screen constraints.
    pub fn prepare_for_peer(
        &self,
        peer: CertificateFingerprint,
        constraints: BoundaryMapConstraints,
    ) -> io::Result<PreparedBoundaryMap> {
        let mut map = self.clipped_to_screen(constraints)?;
        let peer = peer.to_string();
        map.segments
            .retain(|segment| segment.peer_fingerprint == peer);
        Ok(PreparedBoundaryMap {
            map,
            screen: constraints.screen,
        })
    }

    /// Loads the bounded V2 map and rejects unsupported or corrupt data.
    ///
    /// # Errors
    ///
    /// Returns an error for missing, oversized, malformed, or invalid map data.
    pub fn load(store: &DeviceStore) -> io::Result<Self> {
        let path = boundary_map_path(store);
        let file = OpenOptions::new().read(true).open(path)?;
        let metadata = file.metadata()?;
        if metadata.len() == 0 || metadata.len() > BOUNDARY_MAP_FILE_LIMIT {
            return Err(invalid_data(
                "boundary map file length is outside the allowed bounds",
            ));
        }
        let mut bytes = Vec::new();
        file.take(BOUNDARY_MAP_FILE_LIMIT + 1)
            .read_to_end(&mut bytes)?;
        Self::decode(&bytes)
    }

    /// Loads an existing map or returns an empty revision-zero V2 map.
    ///
    /// # Errors
    ///
    /// Corrupt or inaccessible existing files are reported instead of silently reset.
    pub fn load_or_default(store: &DeviceStore) -> io::Result<Self> {
        match Self::load(store) {
            Ok(map) => Ok(map),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error),
        }
    }

    /// Validates a proposed complete replacement, advances its revision, and writes it atomically.
    ///
    /// The proposed revision must equal the current persisted revision so a stale settings page
    /// cannot overwrite a newer edit.
    ///
    /// # Errors
    ///
    /// Returns an error for stale revisions, invalid geometry, corrupt existing data, or I/O
    /// failures. The existing file is left untouched when validation fails.
    pub fn replace(
        store: &DeviceStore,
        mut proposed: Self,
        constraints: BoundaryMapConstraints,
    ) -> io::Result<Self> {
        let current = Self::load_or_default(store)?;
        if proposed.revision != current.revision {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "boundary map edit is based on a stale revision",
            ));
        }
        proposed.canonicalize_peer_fingerprints()?;
        proposed.validate_for_screen(constraints)?;
        proposed.revision = proposed
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid_data("boundary map revision is exhausted"))?;
        let bytes = serde_json::to_vec_pretty(&proposed).map_err(io::Error::other)?;
        if bytes.is_empty()
            || u64::try_from(bytes.len()).unwrap_or(u64::MAX) > BOUNDARY_MAP_FILE_LIMIT
        {
            return Err(invalid_data("encoded boundary map exceeds its size limit"));
        }
        fs::create_dir_all(store.root())?;
        write_atomic(
            &boundary_map_path(store),
            &store.root().join(BOUNDARY_MAP_NEXT_FILE),
            &bytes,
        )?;
        Ok(proposed)
    }

    /// Validates normalized geometry that is independent of the current display resolution.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, invalid identifiers, malformed groups,
    /// duplicate identifiers, excessive peers or segments, and overlapping intervals.
    pub fn validate_schema(&self) -> io::Result<()> {
        if self.schema_version != BOUNDARY_MAP_SCHEMA_VERSION {
            return Err(invalid_data("unsupported boundary map schema version"));
        }
        if self.revision == u64::MAX {
            return Err(invalid_data(
                "boundary map revision uses the reserved maximum",
            ));
        }
        if self.segments.len() > MAX_BOUNDARY_SEGMENTS {
            return Err(invalid_data("boundary map contains too many segments"));
        }
        let mut ids = HashSet::new();
        let mut peers = HashSet::new();
        let mut groups: HashMap<&str, Vec<&BoundarySegment>> = HashMap::new();
        for segment in &self.segments {
            validate_identifier(&segment.id, "segment")?;
            if !ids.insert(segment.id.as_str()) {
                return Err(invalid_data("boundary map contains a duplicate segment id"));
            }
            let peer = CertificateFingerprint::from_str(&segment.peer_fingerprint)?;
            peers.insert(peer);
            if segment.start >= segment.end || segment.end > NORMALIZED_EDGE_LENGTH {
                return Err(invalid_data("boundary segment interval is invalid"));
            }
            match (&segment.group_id, segment.corner) {
                (Some(group_id), Some(_)) => {
                    validate_identifier(group_id, "boundary group")?;
                    groups.entry(group_id).or_default().push(segment);
                }
                (None, None) => {}
                _ => {
                    return Err(invalid_data(
                        "cross-corner segment must contain both group id and corner",
                    ));
                }
            }
        }
        if peers.len() > MAX_BOUNDARY_PEERS {
            return Err(invalid_data(
                "boundary map contains more than five remote peers",
            ));
        }
        validate_groups(&groups)?;
        validate_no_overlaps(&self.segments)
    }

    /// Validates the map against one physical primary-screen size.
    ///
    /// # Errors
    ///
    /// Returns an error when a segment enters a corner guard, is shorter than the physical
    /// minimum, or a cross-corner group is not anchored to the two protected corner boundaries.
    pub fn validate_for_screen(&self, constraints: BoundaryMapConstraints) -> io::Result<()> {
        self.validate_schema()?;
        constraints.validate()?;
        for segment in &self.segments {
            let (safe_start, safe_end) = constraints.normalized_safe_range(segment.edge)?;
            if segment.start < safe_start || segment.end > safe_end {
                return Err(invalid_data("boundary segment enters a protected corner"));
            }
            if !span_meets_physical_minimum(segment, constraints) {
                return Err(invalid_data(
                    "boundary segment is shorter than the physical minimum",
                ));
            }
            if let Some(corner) = segment.corner {
                validate_corner_anchor(segment, corner, safe_start, safe_end)?;
            }
        }
        Ok(())
    }

    /// Produces a fail-closed effective map after a display or guard-size change.
    ///
    /// Segments are clipped to the usable range. A too-short standalone segment is removed; if
    /// either half of a cross-corner group becomes too short, the complete group is removed.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored schema or the new constraints are invalid.
    pub fn clipped_to_screen(&self, constraints: BoundaryMapConstraints) -> io::Result<Self> {
        self.validate_schema()?;
        constraints.validate()?;
        let mut clipped = self.clone();
        for segment in &mut clipped.segments {
            let (safe_start, safe_end) = constraints.normalized_safe_range(segment.edge)?;
            segment.start = segment.start.max(safe_start);
            segment.end = segment.end.min(safe_end);
            if let Some(corner) = segment.corner {
                anchor_cross_corner_segment(segment, corner, safe_start, safe_end);
            }
        }
        let mut rejected_groups = HashSet::new();
        let mut rejected_segments = HashSet::new();
        for segment in &clipped.segments {
            if segment.start >= segment.end || !span_meets_physical_minimum(segment, constraints) {
                if let Some(group) = &segment.group_id {
                    rejected_groups.insert(group.clone());
                } else {
                    rejected_segments.insert(segment.id.clone());
                }
            }
        }
        clipped.segments.retain(|segment| {
            !rejected_segments.contains(&segment.id)
                && segment
                    .group_id
                    .as_ref()
                    .is_none_or(|group| !rejected_groups.contains(group))
        });
        clipped.validate_for_screen(constraints)?;
        Ok(clipped)
    }

    /// Finds the configured peer for one exact physical edge position.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid configuration or an out-of-range physical offset.
    pub fn hit_test_edge(
        &self,
        edge: ScreenBoundary,
        physical_offset: u32,
        constraints: BoundaryMapConstraints,
    ) -> io::Result<Option<BoundaryHit>> {
        self.validate_for_screen(constraints)?;
        hit_test_validated(self, edge, physical_offset, constraints.screen)
    }

    /// Finds the first configured boundary crossed by a fast, possibly diagonal pointer motion.
    ///
    /// The starting point must be inside the primary screen. Exact-corner ties fail closed unless
    /// both edge hits belong to the same logical cross-corner group.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid geometry, an outside starting point, or an ambiguous corner.
    pub fn hit_test_motion(
        &self,
        motion: PointerMotion,
        constraints: BoundaryMapConstraints,
    ) -> io::Result<Option<BoundaryHit>> {
        self.validate_for_screen(constraints)?;
        validate_motion_start(motion, constraints.screen)?;
        let candidates = first_crossings(motion, constraints.screen);
        let mut hits = Vec::new();
        for candidate in candidates {
            if let Some(hit) =
                hit_test_validated(self, candidate.edge, candidate.offset, constraints.screen)?
            {
                hits.push(hit);
            }
        }
        resolve_hits(&hits)
    }

    /// Exchanges every segment assignment for two peers without changing geometry.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid fingerprints, identical peers, or a peer absent from the map.
    pub fn swap_peer_assignments(&mut self, first: &str, second: &str) -> io::Result<()> {
        let first = CertificateFingerprint::from_str(first)?;
        let second = CertificateFingerprint::from_str(second)?;
        if first == second {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot swap one boundary peer with itself",
            ));
        }
        let first_label = first.to_string();
        let second_label = second.to_string();
        let mut saw_first = false;
        let mut saw_second = false;
        for segment in &mut self.segments {
            let peer = CertificateFingerprint::from_str(&segment.peer_fingerprint)?;
            if peer == first {
                segment.peer_fingerprint.clone_from(&second_label);
                saw_first = true;
            } else if peer == second {
                segment.peer_fingerprint.clone_from(&first_label);
                saw_second = true;
            }
        }
        if !saw_first || !saw_second {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "both boundary peers must exist before swapping",
            ));
        }
        Ok(())
    }

    fn decode(bytes: &[u8]) -> io::Result<Self> {
        let mut map: Self = serde_json::from_slice(bytes)
            .map_err(|error| invalid_data(format!("invalid boundary map JSON: {error}")))?;
        map.canonicalize_peer_fingerprints()?;
        map.validate_schema()?;
        Ok(map)
    }

    fn canonicalize_peer_fingerprints(&mut self) -> io::Result<()> {
        for segment in &mut self.segments {
            segment.peer_fingerprint =
                CertificateFingerprint::from_str(&segment.peer_fingerprint)?.to_string();
        }
        Ok(())
    }
}

fn resolve_hits(hits: &[BoundaryHit]) -> io::Result<Option<BoundaryHit>> {
    match hits {
        [] => Ok(None),
        [hit] => Ok(Some(hit.clone())),
        [first, rest @ ..]
            if first.group_id.is_some()
                && rest.iter().all(|hit| {
                    hit.peer_fingerprint == first.peer_fingerprint && hit.group_id == first.group_id
                }) =>
        {
            Ok(Some(first.clone()))
        }
        _ => Err(invalid_data(
            "diagonal motion reaches an ambiguous screen corner",
        )),
    }
}

#[derive(Clone, Copy)]
struct Crossing {
    edge: ScreenBoundary,
    numerator: i128,
    denominator: i128,
    offset: u32,
}

fn first_crossings(motion: PointerMotion, screen: PhysicalScreenSize) -> Vec<Crossing> {
    let maximum_x = i64::from(screen.width - 1);
    let maximum_y = i64::from(screen.height - 1);
    let delta_x = motion.to_x - motion.from_x;
    let delta_y = motion.to_y - motion.from_y;
    let mut candidates = Vec::new();
    if delta_x < 0 && motion.to_x <= 0 {
        push_crossing(
            &mut candidates,
            ScreenBoundary::Left,
            motion.from_x,
            -delta_x,
            motion.from_y,
            delta_y,
            screen.height,
        );
    }
    if delta_x > 0 && motion.to_x >= maximum_x {
        push_crossing(
            &mut candidates,
            ScreenBoundary::Right,
            maximum_x - motion.from_x,
            delta_x,
            motion.from_y,
            delta_y,
            screen.height,
        );
    }
    if delta_y < 0 && motion.to_y <= 0 {
        push_crossing(
            &mut candidates,
            ScreenBoundary::Top,
            motion.from_y,
            -delta_y,
            motion.from_x,
            delta_x,
            screen.width,
        );
    }
    if delta_y > 0 && motion.to_y >= maximum_y {
        push_crossing(
            &mut candidates,
            ScreenBoundary::Bottom,
            maximum_y - motion.from_y,
            delta_y,
            motion.from_x,
            delta_x,
            screen.width,
        );
    }
    let Some(earliest) = candidates.iter().min_by(|first, second| {
        (first.numerator * second.denominator).cmp(&(second.numerator * first.denominator))
    }) else {
        return Vec::new();
    };
    candidates
        .iter()
        .copied()
        .filter(|candidate| {
            candidate.numerator * earliest.denominator == earliest.numerator * candidate.denominator
        })
        .collect()
}

fn push_crossing(
    candidates: &mut Vec<Crossing>,
    edge: ScreenBoundary,
    numerator: i64,
    denominator: i64,
    perpendicular_origin: i64,
    perpendicular_delta: i64,
    perpendicular_extent: u32,
) {
    if numerator < 0 || denominator <= 0 || numerator > denominator {
        return;
    }
    let numerator = i128::from(numerator);
    let denominator = i128::from(denominator);
    let coordinate_numerator = i128::from(perpendicular_origin) * denominator
        + i128::from(perpendicular_delta) * numerator;
    let coordinate = divide_round_nearest(coordinate_numerator, denominator);
    if coordinate < 0 || coordinate >= i128::from(perpendicular_extent) {
        return;
    }
    candidates.push(Crossing {
        edge,
        numerator,
        denominator,
        offset: u32::try_from(coordinate).expect("screen crossing offset fits in u32"),
    });
}

fn divide_round_nearest(numerator: i128, denominator: i128) -> i128 {
    if numerator >= 0 {
        (numerator + denominator / 2) / denominator
    } else {
        (numerator - denominator / 2) / denominator
    }
}

fn validate_motion_start(motion: PointerMotion, screen: PhysicalScreenSize) -> io::Result<()> {
    if motion.from_x < 0
        || motion.from_y < 0
        || motion.from_x >= i64::from(screen.width)
        || motion.from_y >= i64::from(screen.height)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "pointer motion must start inside the primary screen",
        ));
    }
    Ok(())
}

fn validate_pointer_sample(sample: PointerSample, screen: PhysicalScreenSize) -> io::Result<()> {
    if sample.position.x >= screen.width || sample.position.y >= screen.height {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "pointer sample is outside the primary screen",
        ));
    }
    Ok(())
}

fn hit_test_validated(
    map: &BoundaryMapV2,
    edge: ScreenBoundary,
    physical_offset: u32,
    screen: PhysicalScreenSize,
) -> io::Result<Option<BoundaryHit>> {
    let extent = edge_extent(edge, screen);
    let normalized = physical_boundary_to_normalized(physical_offset, extent)?;
    Ok(map
        .segments
        .iter()
        .find(|segment| {
            segment.edge == edge && segment.start <= normalized && normalized < segment.end
        })
        .map(|segment| BoundaryHit {
            segment_id: segment.id.clone(),
            peer_fingerprint: segment.peer_fingerprint.clone(),
            edge,
            normalized_offset: normalized,
            group_id: segment.group_id.clone(),
        }))
}

fn physical_boundary_to_normalized(offset: u32, extent: u32) -> io::Result<u32> {
    if offset >= extent {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "physical boundary offset is outside the screen edge",
        ));
    }
    Ok(
        u32::try_from(u64::from(offset) * u64::from(NORMALIZED_EDGE_LENGTH) / u64::from(extent))
            .expect("normalized boundary offset fits in u32"),
    )
}

fn span_meets_physical_minimum(
    segment: &BoundarySegment,
    constraints: BoundaryMapConstraints,
) -> bool {
    u64::from(segment.span()) * u64::from(edge_extent(segment.edge, constraints.screen))
        >= u64::from(constraints.minimum_segment_pixels) * u64::from(NORMALIZED_EDGE_LENGTH)
}

fn validate_groups(groups: &HashMap<&str, Vec<&BoundarySegment>>) -> io::Result<()> {
    for segments in groups.values() {
        if segments.len() != 2 {
            return Err(invalid_data(
                "cross-corner group must contain exactly two segments",
            ));
        }
        let first = segments[0];
        let second = segments[1];
        if first.peer_fingerprint != second.peer_fingerprint || first.corner != second.corner {
            return Err(invalid_data(
                "cross-corner group must use one peer and one corner",
            ));
        }
        let expected = first
            .corner
            .expect("grouped segments contain a corner")
            .expected_edges();
        if !expected.contains(&first.edge)
            || !expected.contains(&second.edge)
            || first.edge == second.edge
        {
            return Err(invalid_data(
                "cross-corner group does not occupy its two adjacent edges",
            ));
        }
    }
    Ok(())
}

fn validate_corner_anchor(
    segment: &BoundarySegment,
    corner: BoundaryCorner,
    safe_start: u32,
    safe_end: u32,
) -> io::Result<()> {
    let anchored = match (corner, segment.edge) {
        (BoundaryCorner::TopLeft, ScreenBoundary::Top | ScreenBoundary::Left)
        | (BoundaryCorner::TopRight, ScreenBoundary::Right)
        | (BoundaryCorner::BottomLeft, ScreenBoundary::Bottom) => segment.start == safe_start,
        (BoundaryCorner::TopRight, ScreenBoundary::Top)
        | (BoundaryCorner::BottomRight, ScreenBoundary::Bottom | ScreenBoundary::Right)
        | (BoundaryCorner::BottomLeft, ScreenBoundary::Left) => segment.end == safe_end,
        _ => false,
    };
    if !anchored {
        return Err(invalid_data(
            "cross-corner segment is not anchored to the corner guard",
        ));
    }
    Ok(())
}

fn anchor_cross_corner_segment(
    segment: &mut BoundarySegment,
    corner: BoundaryCorner,
    safe_start: u32,
    safe_end: u32,
) {
    match (corner, segment.edge) {
        (BoundaryCorner::TopLeft, ScreenBoundary::Top | ScreenBoundary::Left)
        | (BoundaryCorner::TopRight, ScreenBoundary::Right)
        | (BoundaryCorner::BottomLeft, ScreenBoundary::Bottom) => segment.start = safe_start,
        (BoundaryCorner::TopRight, ScreenBoundary::Top)
        | (BoundaryCorner::BottomRight, ScreenBoundary::Bottom | ScreenBoundary::Right)
        | (BoundaryCorner::BottomLeft, ScreenBoundary::Left) => segment.end = safe_end,
        _ => {}
    }
}

fn validate_no_overlaps(segments: &[BoundarySegment]) -> io::Result<()> {
    for edge in all_edges() {
        let mut on_edge = segments
            .iter()
            .filter(|segment| segment.edge == edge)
            .collect::<Vec<_>>();
        on_edge.sort_by_key(|segment| (segment.start, segment.end));
        for pair in on_edge.windows(2) {
            if pair[0].end > pair[1].start {
                return Err(invalid_data("boundary segments overlap on one screen edge"));
            }
        }
    }
    Ok(())
}

fn validate_identifier(value: &str, kind: &str) -> io::Result<()> {
    if value.is_empty()
        || value.len() > IDENTIFIER_BYTE_LIMIT
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(invalid_data(format!("{kind} id is invalid")));
    }
    Ok(())
}

const fn all_edges() -> [ScreenBoundary; 4] {
    [
        ScreenBoundary::Top,
        ScreenBoundary::Right,
        ScreenBoundary::Bottom,
        ScreenBoundary::Left,
    ]
}

const fn edge_extent(edge: ScreenBoundary, screen: PhysicalScreenSize) -> u32 {
    match edge {
        ScreenBoundary::Top | ScreenBoundary::Bottom => screen.width,
        ScreenBoundary::Left | ScreenBoundary::Right => screen.height,
    }
}

fn boundary_map_path(store: &DeviceStore) -> std::path::PathBuf {
    store.root().join(BOUNDARY_MAP_FILE)
}

fn write_atomic(path: &Path, temporary: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    replace_file(temporary, path)
}

fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
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
    unsafe {
        MoveFileExW(
            windows_core::PCWSTR(from.as_ptr()),
            windows_core::PCWSTR(to.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(io::Error::other)
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static DIRECTORY_SERIAL: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(std::path::PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let serial = DIRECTORY_SERIAL.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "pierdesk-boundary-map-{}-{serial}",
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

    fn fingerprint(byte: u8) -> String {
        CertificateFingerprint::from_bytes([byte; 32]).to_string()
    }

    fn screen(width: u32, height: u32) -> PhysicalScreenSize {
        PhysicalScreenSize::new(width, height).unwrap()
    }

    fn constraints(width: u32, height: u32, guard: u32) -> BoundaryMapConstraints {
        BoundaryMapConstraints {
            screen: screen(width, height),
            corner_guard_enabled: true,
            corner_guard_pixels: guard,
            minimum_segment_pixels: DEFAULT_MINIMUM_SEGMENT_PIXELS,
        }
    }

    fn segment(id: &str, peer: u8, edge: ScreenBoundary, start: u32, end: u32) -> BoundarySegment {
        BoundarySegment {
            id: id.to_owned(),
            peer_fingerprint: fingerprint(peer),
            edge,
            start,
            end,
            group_id: None,
            corner: None,
        }
    }

    #[test]
    fn adjacent_half_open_segments_share_a_divider_without_overlap() {
        let constraints = constraints(1920, 1080, 32);
        let (safe_start, safe_end) = constraints
            .normalized_safe_range(ScreenBoundary::Top)
            .unwrap();
        let divider = NORMALIZED_EDGE_LENGTH / 2;
        let map = BoundaryMapV2 {
            segments: vec![
                segment("b-top", 1, ScreenBoundary::Top, safe_start, divider),
                segment("c-top", 2, ScreenBoundary::Top, divider, safe_end),
            ],
            ..BoundaryMapV2::default()
        };
        map.validate_for_screen(constraints).unwrap();
        assert_eq!(
            map.hit_test_edge(ScreenBoundary::Top, 959, constraints)
                .unwrap()
                .unwrap()
                .peer_fingerprint,
            fingerprint(1)
        );
        assert_eq!(
            map.hit_test_edge(ScreenBoundary::Top, 960, constraints)
                .unwrap()
                .unwrap()
                .peer_fingerprint,
            fingerprint(2)
        );
    }

    #[test]
    fn prepared_map_filters_peers_and_unassigned_edge_positions_fail_closed() {
        let constraints = constraints(1920, 1080, 32);
        let map = BoundaryMapV2 {
            segments: vec![
                segment("peer-one-right", 1, ScreenBoundary::Right, 8_000, 24_000),
                segment("peer-two-top", 2, ScreenBoundary::Top, 24_000, 48_000),
            ],
            ..BoundaryMapV2::default()
        };
        let prepared = map
            .prepare_for_peer(CertificateFingerprint::from_bytes([1; 32]), constraints)
            .unwrap();
        assert_eq!(prepared.segment_count(), 1);
        assert_eq!(
            prepared
                .hit_test_sample(PointerSample {
                    position: super::super::PhysicalPoint { x: 1919, y: 270 },
                    delta_x: 1,
                    delta_y: 0,
                })
                .unwrap()
                .unwrap()
                .edge,
            ScreenBoundary::Right
        );
        assert!(
            prepared
                .hit_test_sample(PointerSample {
                    position: super::super::PhysicalPoint { x: 1919, y: 700 },
                    delta_x: 1,
                    delta_y: 0,
                })
                .unwrap()
                .is_none()
        );
        assert!(
            prepared
                .hit_test_sample(PointerSample {
                    position: super::super::PhysicalPoint { x: 960, y: 0 },
                    delta_x: 0,
                    delta_y: -1,
                })
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn legacy_migration_map_uses_only_the_guard_safe_part_of_the_old_edge() {
        let constraints = constraints(1920, 1080, 32);
        let peer = CertificateFingerprint::from_bytes([9; 32]);
        let map = BoundaryMapV2::legacy_full_edge(peer, ScreenBoundary::Top, constraints).unwrap();
        let (safe_start, safe_end) = constraints
            .normalized_safe_range(ScreenBoundary::Top)
            .unwrap();
        assert_eq!(map.segments.len(), 1);
        assert_eq!(map.segments[0].peer_fingerprint, peer.to_string());
        assert_eq!(
            (map.segments[0].start, map.segments[0].end),
            (safe_start, safe_end)
        );
        map.validate_for_screen(constraints).unwrap();
    }

    #[test]
    fn overlap_guard_and_minimum_length_fail_closed() {
        let constraints = constraints(1920, 1080, 32);
        let mut map = BoundaryMapV2 {
            segments: vec![
                segment("one", 1, ScreenBoundary::Right, 2_000, 20_000),
                segment("two", 2, ScreenBoundary::Right, 19_999, 40_000),
            ],
            ..BoundaryMapV2::default()
        };
        assert!(map.validate_for_screen(constraints).is_err());
        map.segments.pop();
        map.segments[0].start = 0;
        assert!(map.validate_for_screen(constraints).is_err());
        let (safe_start, _) = constraints
            .normalized_safe_range(ScreenBoundary::Right)
            .unwrap();
        map.segments[0].start = safe_start;
        map.segments[0].end = safe_start + 100;
        assert!(map.validate_for_screen(constraints).is_err());
    }

    #[test]
    fn cross_corner_group_reanchors_after_resolution_change() {
        let original = constraints(1920, 1080, 32);
        let (top_start, top_end) = original.normalized_safe_range(ScreenBoundary::Top).unwrap();
        let (right_start, _) = original
            .normalized_safe_range(ScreenBoundary::Right)
            .unwrap();
        let mut top = segment("corner-top", 3, ScreenBoundary::Top, 30_000, top_end);
        top.group_id = Some("corner-tr".to_owned());
        top.corner = Some(BoundaryCorner::TopRight);
        let mut right = segment(
            "corner-right",
            3,
            ScreenBoundary::Right,
            right_start,
            35_000,
        );
        right.group_id = Some("corner-tr".to_owned());
        right.corner = Some(BoundaryCorner::TopRight);
        let map = BoundaryMapV2 {
            segments: vec![top, right],
            ..BoundaryMapV2::default()
        };
        map.validate_for_screen(original).unwrap();

        let resized = constraints(1280, 720, 32);
        assert!(map.validate_for_screen(resized).is_err());
        let clipped = map.clipped_to_screen(resized).unwrap();
        clipped.validate_for_screen(resized).unwrap();
        let (_, resized_top_end) = resized.normalized_safe_range(ScreenBoundary::Top).unwrap();
        let (resized_right_start, _) = resized
            .normalized_safe_range(ScreenBoundary::Right)
            .unwrap();
        assert_eq!(clipped.segments[0].end, resized_top_end);
        assert_eq!(clipped.segments[1].start, resized_right_start);
        assert!(top_start < top_end);
    }

    #[test]
    fn rapid_diagonal_motion_uses_the_first_intersection_and_blocks_corners() {
        let constraints = constraints(1000, 800, 32);
        let (safe_start, safe_end) = constraints
            .normalized_safe_range(ScreenBoundary::Top)
            .unwrap();
        let map = BoundaryMapV2 {
            segments: vec![segment("top", 4, ScreenBoundary::Top, safe_start, safe_end)],
            ..BoundaryMapV2::default()
        };
        let hit = map
            .hit_test_motion(
                PointerMotion {
                    from_x: 500,
                    from_y: 400,
                    to_x: 900,
                    to_y: -400,
                },
                constraints,
            )
            .unwrap()
            .unwrap();
        assert_eq!(hit.edge, ScreenBoundary::Top);
        assert_eq!(hit.peer_fingerprint, fingerprint(4));

        assert!(
            map.hit_test_motion(
                PointerMotion {
                    from_x: 500,
                    from_y: 400,
                    to_x: 1_000,
                    to_y: 0,
                },
                constraints,
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn cross_corner_tie_is_one_logical_hit_for_one_group() {
        let constraints = BoundaryMapConstraints {
            screen: screen(1000, 800),
            corner_guard_enabled: false,
            corner_guard_pixels: 0,
            minimum_segment_pixels: 64,
        };
        let mut top = segment(
            "top-right-top",
            5,
            ScreenBoundary::Top,
            40_000,
            NORMALIZED_EDGE_LENGTH,
        );
        top.group_id = Some("top-right".to_owned());
        top.corner = Some(BoundaryCorner::TopRight);
        let mut right = segment("top-right-right", 5, ScreenBoundary::Right, 0, 30_000);
        right.group_id = Some("top-right".to_owned());
        right.corner = Some(BoundaryCorner::TopRight);
        let map = BoundaryMapV2 {
            segments: vec![top, right],
            ..BoundaryMapV2::default()
        };
        let hit = map
            .hit_test_motion(
                PointerMotion {
                    from_x: 500,
                    from_y: 400,
                    to_x: 1_000,
                    to_y: 0,
                },
                constraints,
            )
            .unwrap()
            .unwrap();
        assert_eq!(hit.group_id.as_deref(), Some("top-right"));
    }

    #[test]
    fn peer_assignment_swap_preserves_geometry() {
        let mut map = BoundaryMapV2 {
            segments: vec![
                segment("one", 1, ScreenBoundary::Left, 5_000, 20_000),
                segment("two", 2, ScreenBoundary::Right, 25_000, 40_000),
            ],
            ..BoundaryMapV2::default()
        };
        let geometry = map
            .segments
            .iter()
            .map(|segment| (segment.edge, segment.start, segment.end))
            .collect::<Vec<_>>();
        map.swap_peer_assignments(&fingerprint(1), &fingerprint(2))
            .unwrap();
        assert_eq!(map.segments[0].peer_fingerprint, fingerprint(2));
        assert_eq!(map.segments[1].peer_fingerprint, fingerprint(1));
        assert_eq!(
            map.segments
                .iter()
                .map(|segment| (segment.edge, segment.start, segment.end))
                .collect::<Vec<_>>(),
            geometry
        );
    }

    #[test]
    fn durable_replacement_round_trips_and_rejects_stale_revision() {
        let directory = TestDirectory::new();
        let store = DeviceStore::new(&directory.0);
        let constraints = constraints(1920, 1080, 32);
        let (safe_start, safe_end) = constraints
            .normalized_safe_range(ScreenBoundary::Bottom)
            .unwrap();
        let proposed = BoundaryMapV2 {
            segments: vec![segment(
                "bottom",
                6,
                ScreenBoundary::Bottom,
                safe_start,
                safe_end,
            )],
            ..BoundaryMapV2::default()
        };
        let saved = BoundaryMapV2::replace(&store, proposed.clone(), constraints).unwrap();
        assert_eq!(saved.revision, 1);
        assert_eq!(BoundaryMapV2::load(&store).unwrap(), saved);
        assert!(BoundaryMapV2::replace(&store, proposed, constraints).is_err());
    }

    #[test]
    fn corrupt_unknown_and_oversized_files_are_never_reset_silently() {
        let directory = TestDirectory::new();
        let store = DeviceStore::new(&directory.0);
        fs::write(boundary_map_path(&store), b"not json").unwrap();
        assert!(BoundaryMapV2::load_or_default(&store).is_err());
        fs::write(
            boundary_map_path(&store),
            br#"{"schemaVersion":2,"revision":0,"segments":[],"unknown":true}"#,
        )
        .unwrap();
        assert!(BoundaryMapV2::load(&store).is_err());
        fs::write(
            boundary_map_path(&store),
            vec![b'x'; usize::try_from(BOUNDARY_MAP_FILE_LIMIT + 1).unwrap()],
        )
        .unwrap();
        assert!(BoundaryMapV2::load(&store).is_err());
    }
}
