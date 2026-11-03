use std::collections::{HashMap, HashSet};
use std::io;
use std::str::FromStr as _;

use serde::{Deserialize, Serialize};

use super::{PhysicalScreenSize, ScreenBoundary};
use crate::security::CertificateFingerprint;

pub const LOGICAL_BOUNDARY_SCHEMA_VERSION: u16 = 3;
pub const NORMALIZED_PORTAL_LENGTH: u32 = 65_536;
pub const MAX_WORKSPACE_SCREENS: usize = 6;
pub const MAX_LOGICAL_BOUNDARIES: usize = 32;
pub const MAX_PORTAL_SEGMENTS: usize = 5;

const IDENTIFIER_BYTE_LIMIT: usize = 64;
const MAX_PHYSICAL_SCREEN_MILLIMETERS: u32 = 10_000;
const MAX_CANVAS_COORDINATE_MILLIMETERS: u32 = 1_000_000;
const NORMALIZED_PERIMETER_LENGTH: u64 = NORMALIZED_PORTAL_LENGTH as u64 * 4;
const PARAMETER_SCALE: u128 = u16::MAX as u128 + 1;

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BoundaryLayoutV3 {
    pub schema_version: u16,
    pub revision: u64,
    pub local_fingerprint: String,
    pub screens: Vec<BoundaryScreen>,
    pub boundaries: Vec<LogicalBoundary>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BoundaryScreen {
    pub device_fingerprint: String,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub physical_width_millimeters: u32,
    pub physical_height_millimeters: u32,
    pub rotation_quarter_turns: u8,
    pub canvas_x_millimeters: i32,
    pub canvas_y_millimeters: i32,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct LogicalBoundary {
    pub id: String,
    pub peer_fingerprint: String,
    pub mapping_revision: u64,
    pub local_confirmed_revision: u64,
    pub peer_confirmed_revision: u64,
    pub reversed: bool,
    pub source: BoundaryPortal,
    pub target: BoundaryPortal,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BoundaryPortal {
    pub segments: Vec<BoundaryArcSegment>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BoundaryArcSegment {
    pub edge: ScreenBoundary,
    pub start: u32,
    pub end: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PortalPoint {
    pub edge: ScreenBoundary,
    pub normalized_offset: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PortalTransfer {
    /// The cumulative-length parameter on the departing portal before direction reversal.
    pub parameter: u16,
    pub destination: PortalPoint,
}

#[derive(Clone, Debug)]
pub struct CompiledBoundaryLayout {
    revision: u64,
    boundaries: Vec<CompiledLogicalBoundary>,
}

#[derive(Clone, Debug)]
pub struct CompiledLogicalBoundary {
    boundary_id: String,
    peer_fingerprint: String,
    mapping_revision: u64,
    reversed: bool,
    source: CompiledPortal,
    target: CompiledPortal,
}

#[derive(Clone, Debug)]
pub struct CompiledPortal {
    segments: Vec<BoundaryArcSegment>,
    pieces: Vec<WeightedPiece>,
    total_physical_weight: u128,
}

#[derive(Clone, Copy, Debug)]
struct WeightedPiece {
    segment: BoundaryArcSegment,
    clockwise_forward: bool,
    physical_extent_millimeters: u32,
    physical_weight: u128,
}

#[derive(Clone, Copy, Debug)]
struct OrderedSegment {
    segment: BoundaryArcSegment,
    perimeter_start: u64,
    perimeter_end: u64,
    clockwise_forward: bool,
}

impl BoundaryLayoutV3 {
    /// Rewrites every stored device fingerprint into the canonical wire representation.
    ///
    /// # Errors
    ///
    /// Returns an error when any local, screen, or boundary peer fingerprint is malformed.
    pub fn canonicalize_fingerprints(&mut self) -> io::Result<()> {
        self.local_fingerprint = canonical_fingerprint(&self.local_fingerprint)?;
        for screen in &mut self.screens {
            screen.device_fingerprint = canonical_fingerprint(&screen.device_fingerprint)?;
        }
        for boundary in &mut self.boundaries {
            boundary.peer_fingerprint = canonical_fingerprint(&boundary.peer_fingerprint)?;
        }
        Ok(())
    }

    /// Validates the authoring model without requiring both devices to have confirmed it.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, malformed screens or portals, duplicate ids,
    /// missing peers, overlapping entrances, or invalid confirmation revisions.
    pub fn validate_schema(&self) -> io::Result<()> {
        if self.schema_version != LOGICAL_BOUNDARY_SCHEMA_VERSION {
            return Err(invalid_data("unsupported logical boundary schema version"));
        }
        if self.revision == u64::MAX {
            return Err(invalid_data(
                "logical boundary layout revision uses the reserved maximum",
            ));
        }
        if self.screens.is_empty() || self.screens.len() > MAX_WORKSPACE_SCREENS {
            return Err(invalid_data(
                "logical boundary layout must contain between one and six screens",
            ));
        }
        if self.boundaries.len() > MAX_LOGICAL_BOUNDARIES {
            return Err(invalid_data(
                "logical boundary layout contains too many boundaries",
            ));
        }

        let local = canonical_fingerprint(&self.local_fingerprint)?;
        let mut screen_ids = HashSet::new();
        for screen in &self.screens {
            screen.validate()?;
            let fingerprint = canonical_fingerprint(&screen.device_fingerprint)?;
            if !screen_ids.insert(fingerprint) {
                return Err(invalid_data(
                    "logical boundary layout contains a duplicate screen",
                ));
            }
        }
        if !screen_ids.contains(&local) {
            return Err(invalid_data(
                "logical boundary layout does not contain the local screen",
            ));
        }

        let mut boundary_ids = HashSet::new();
        let mut peers = HashSet::new();
        for boundary in &self.boundaries {
            validate_identifier(&boundary.id, "logical boundary")?;
            if !boundary_ids.insert(boundary.id.as_str()) {
                return Err(invalid_data(
                    "logical boundary layout contains a duplicate boundary id",
                ));
            }
            let peer = canonical_fingerprint(&boundary.peer_fingerprint)?;
            if peer == local {
                return Err(invalid_data(
                    "logical boundary cannot target the local screen",
                ));
            }
            if !screen_ids.contains(&peer) {
                return Err(invalid_data(
                    "logical boundary targets a screen outside the workspace",
                ));
            }
            peers.insert(peer);
            boundary.validate()?;
        }
        if peers.len() > MAX_WORKSPACE_SCREENS - 1 {
            return Err(invalid_data(
                "logical boundary layout contains more than five remote peers",
            ));
        }
        validate_portal_overlaps(&self.boundaries)
    }

    /// Compiles a confirmed authoring layout into immutable, guard-safe portal geometry.
    ///
    /// Corner guards are applied exactly once here. The resulting portal parameters use the
    /// retained physical lengths, so a mapped destination can never land inside a removed guard.
    ///
    /// # Errors
    ///
    /// Returns an error when the authoring schema is invalid, any mapping revision is not
    /// confirmed by both devices, a guard consumes a screen edge, or an effective portal is too
    /// short.
    pub fn compile(
        &self,
        corner_guard_pixels: u32,
        minimum_portal_pixels: u32,
    ) -> io::Result<CompiledBoundaryLayout> {
        self.validate_schema()?;
        if minimum_portal_pixels == 0 {
            return Err(invalid_data(
                "minimum effective portal length must be positive",
            ));
        }

        let screens = self
            .screens
            .iter()
            .map(|screen| {
                canonical_fingerprint(&screen.device_fingerprint)
                    .map(|fingerprint| (fingerprint, screen))
            })
            .collect::<io::Result<HashMap<_, _>>>()?;
        let local = canonical_fingerprint(&self.local_fingerprint)?;
        let local_screen = screens
            .get(&local)
            .copied()
            .ok_or_else(|| invalid_data("local screen is missing during compilation"))?;
        local_screen.validate_guard(corner_guard_pixels)?;

        let mut boundaries = Vec::with_capacity(self.boundaries.len());
        for boundary in &self.boundaries {
            if !boundary.is_confirmed() {
                return Err(invalid_data(format!(
                    "logical boundary {} has an unconfirmed mapping revision",
                    boundary.id
                )));
            }
            let peer = canonical_fingerprint(&boundary.peer_fingerprint)?;
            let target_screen = screens
                .get(&peer)
                .copied()
                .ok_or_else(|| invalid_data("target screen is missing during compilation"))?;
            target_screen.validate_guard(corner_guard_pixels)?;
            boundaries.push(CompiledLogicalBoundary {
                boundary_id: boundary.id.clone(),
                peer_fingerprint: peer,
                mapping_revision: boundary.mapping_revision,
                reversed: boundary.reversed,
                source: CompiledPortal::compile(
                    &boundary.source,
                    local_screen,
                    corner_guard_pixels,
                    minimum_portal_pixels,
                    &boundary.id,
                )?,
                target: CompiledPortal::compile(
                    &boundary.target,
                    target_screen,
                    corner_guard_pixels,
                    minimum_portal_pixels,
                    &boundary.id,
                )?,
            });
        }
        Ok(CompiledBoundaryLayout {
            revision: self.revision,
            boundaries,
        })
    }
}

impl BoundaryScreen {
    /// Returns the current pixel dimensions after the stored quarter-turn rotation.
    ///
    /// # Errors
    ///
    /// Returns an error when the screen descriptor is invalid.
    pub fn oriented_pixel_size(&self) -> io::Result<PhysicalScreenSize> {
        self.validate()?;
        let (width, height) = if self.rotation_quarter_turns.is_multiple_of(2) {
            (self.pixel_width, self.pixel_height)
        } else {
            (self.pixel_height, self.pixel_width)
        };
        PhysicalScreenSize::new(width, height)
    }

    fn validate(&self) -> io::Result<()> {
        canonical_fingerprint(&self.device_fingerprint)?;
        PhysicalScreenSize::new(self.pixel_width, self.pixel_height)?;
        if self.physical_width_millimeters == 0
            || self.physical_height_millimeters == 0
            || self.physical_width_millimeters > MAX_PHYSICAL_SCREEN_MILLIMETERS
            || self.physical_height_millimeters > MAX_PHYSICAL_SCREEN_MILLIMETERS
        {
            return Err(invalid_data(
                "screen physical dimensions must be between 1 and 10000 millimeters",
            ));
        }
        if self.rotation_quarter_turns > 3 {
            return Err(invalid_data(
                "screen rotation must contain zero to three quarter turns",
            ));
        }
        if self.canvas_x_millimeters.unsigned_abs() > MAX_CANVAS_COORDINATE_MILLIMETERS
            || self.canvas_y_millimeters.unsigned_abs() > MAX_CANVAS_COORDINATE_MILLIMETERS
        {
            return Err(invalid_data(
                "screen canvas position is outside the supported range",
            ));
        }
        Ok(())
    }

    fn validate_guard(&self, corner_guard_pixels: u32) -> io::Result<()> {
        let screen = self.oriented_pixel_size()?;
        if corner_guard_pixels.saturating_mul(2) >= screen.width
            || corner_guard_pixels.saturating_mul(2) >= screen.height
        {
            return Err(invalid_data("corner guard consumes a complete screen edge"));
        }
        Ok(())
    }

    const fn oriented_physical_width(&self) -> u32 {
        if self.rotation_quarter_turns.is_multiple_of(2) {
            self.physical_width_millimeters
        } else {
            self.physical_height_millimeters
        }
    }

    const fn oriented_physical_height(&self) -> u32 {
        if self.rotation_quarter_turns.is_multiple_of(2) {
            self.physical_height_millimeters
        } else {
            self.physical_width_millimeters
        }
    }

    const fn physical_extent(&self, edge: ScreenBoundary) -> u32 {
        match edge {
            ScreenBoundary::Top | ScreenBoundary::Bottom => self.oriented_physical_width(),
            ScreenBoundary::Left | ScreenBoundary::Right => self.oriented_physical_height(),
        }
    }
}

impl LogicalBoundary {
    #[must_use]
    pub const fn is_confirmed(&self) -> bool {
        self.local_confirmed_revision == self.mapping_revision
            && self.peer_confirmed_revision == self.mapping_revision
    }

    fn validate(&self) -> io::Result<()> {
        if self.mapping_revision == 0 || self.mapping_revision == u64::MAX {
            return Err(invalid_data(
                "logical boundary mapping revision must be positive and non-reserved",
            ));
        }
        if self.local_confirmed_revision > self.mapping_revision
            || self.peer_confirmed_revision > self.mapping_revision
        {
            return Err(invalid_data(
                "logical boundary confirmation exceeds its mapping revision",
            ));
        }
        ordered_segments(&self.source)?;
        ordered_segments(&self.target)?;
        Ok(())
    }
}

impl CompiledBoundaryLayout {
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    #[must_use]
    pub fn boundaries(&self) -> &[CompiledLogicalBoundary] {
        &self.boundaries
    }

    #[must_use]
    pub fn boundary(&self, boundary_id: &str) -> Option<&CompiledLogicalBoundary> {
        self.boundaries
            .iter()
            .find(|boundary| boundary.boundary_id == boundary_id)
    }

    /// Returns this compiled layout without the boundaries that belong to `peers`.
    ///
    /// Geometry, the layout revision, and every other peer's boundaries are kept unchanged, so a
    /// switched-off pair does not disturb the remaining pairs.
    #[must_use]
    pub fn without_peers(&self, peers: &HashSet<[u8; 32]>) -> Self {
        let boundaries = self
            .boundaries
            .iter()
            .filter(|boundary| {
                CertificateFingerprint::from_str(&boundary.peer_fingerprint)
                    .map_or(true, |peer| !peers.contains(&peer.bytes()))
            })
            .cloned()
            .collect();
        Self {
            revision: self.revision,
            boundaries,
        }
    }
}

impl CompiledLogicalBoundary {
    #[must_use]
    pub fn boundary_id(&self) -> &str {
        &self.boundary_id
    }

    #[must_use]
    pub fn peer_fingerprint(&self) -> &str {
        &self.peer_fingerprint
    }

    #[must_use]
    pub const fn mapping_revision(&self) -> u64 {
        self.mapping_revision
    }

    #[must_use]
    pub const fn reversed(&self) -> bool {
        self.reversed
    }

    #[must_use]
    pub const fn source(&self) -> &CompiledPortal {
        &self.source
    }

    #[must_use]
    pub const fn target(&self) -> &CompiledPortal {
        &self.target
    }

    #[must_use]
    pub fn source_to_target(&self, point: PortalPoint) -> Option<PortalTransfer> {
        let parameter = self.source.parameter_for_point(point)?;
        let target_parameter = if self.reversed {
            u16::MAX - parameter
        } else {
            parameter
        };
        Some(PortalTransfer {
            parameter,
            destination: self.target.point_at_parameter(target_parameter),
        })
    }

    #[must_use]
    pub fn target_to_source(&self, point: PortalPoint) -> Option<PortalTransfer> {
        let parameter = self.target.parameter_for_point(point)?;
        let source_parameter = if self.reversed {
            u16::MAX - parameter
        } else {
            parameter
        };
        Some(PortalTransfer {
            parameter,
            destination: self.source.point_at_parameter(source_parameter),
        })
    }
}

impl CompiledPortal {
    fn compile(
        portal: &BoundaryPortal,
        screen: &BoundaryScreen,
        corner_guard_pixels: u32,
        minimum_portal_pixels: u32,
        boundary_id: &str,
    ) -> io::Result<Self> {
        let ordered = ordered_segments(portal)?;
        let pixel_size = screen.oriented_pixel_size()?;
        let mut effective = Vec::with_capacity(ordered.len());
        for ordered_segment in ordered {
            let (safe_start, safe_end) = normalized_safe_range(
                edge_pixel_extent(ordered_segment.segment.edge, pixel_size),
                corner_guard_pixels,
            );
            let segment = BoundaryArcSegment {
                edge: ordered_segment.segment.edge,
                start: ordered_segment.segment.start.max(safe_start),
                end: ordered_segment.segment.end.min(safe_end),
            };
            if segment.start < segment.end {
                effective.push(OrderedSegment {
                    segment,
                    perimeter_start: ordered_segment.perimeter_start,
                    perimeter_end: ordered_segment.perimeter_end,
                    clockwise_forward: ordered_segment.clockwise_forward,
                });
            }
        }
        if effective.is_empty() {
            return Err(invalid_data(format!(
                "logical boundary {boundary_id} is completely covered by corner guards"
            )));
        }
        let pixel_weight = effective.iter().fold(0_u128, |total, piece| {
            total
                + u128::from(piece.segment.end - piece.segment.start)
                    * u128::from(edge_pixel_extent(piece.segment.edge, pixel_size))
        });
        if pixel_weight < u128::from(minimum_portal_pixels) * u128::from(NORMALIZED_PORTAL_LENGTH) {
            return Err(invalid_data(format!(
                "logical boundary {boundary_id} is shorter than the effective minimum"
            )));
        }

        let pieces = effective
            .iter()
            .map(|piece| {
                let physical_extent_millimeters = screen.physical_extent(piece.segment.edge);
                WeightedPiece {
                    segment: piece.segment,
                    clockwise_forward: piece.clockwise_forward,
                    physical_extent_millimeters,
                    physical_weight: u128::from(piece.segment.end - piece.segment.start)
                        * u128::from(physical_extent_millimeters),
                }
            })
            .collect::<Vec<_>>();
        let total_physical_weight = pieces.iter().map(|piece| piece.physical_weight).sum();
        if total_physical_weight == 0 {
            return Err(invalid_data(format!(
                "logical boundary {boundary_id} has no physical portal length"
            )));
        }
        Ok(Self {
            segments: pieces.iter().map(|piece| piece.segment).collect(),
            pieces,
            total_physical_weight,
        })
    }

    #[must_use]
    pub fn segments(&self) -> &[BoundaryArcSegment] {
        &self.segments
    }

    #[must_use]
    pub fn point_at_parameter(&self, parameter: u16) -> PortalPoint {
        let mut remaining = u128::from(parameter) * self.total_physical_weight / PARAMETER_SCALE;
        for (index, piece) in self.pieces.iter().enumerate() {
            if remaining < piece.physical_weight || index == self.pieces.len() - 1 {
                let span = piece.segment.end - piece.segment.start;
                let delta =
                    u32::try_from(remaining / u128::from(piece.physical_extent_millimeters))
                        .unwrap_or(u32::MAX)
                        .min(span - 1);
                let normalized_offset = if piece.clockwise_forward {
                    piece.segment.start + delta
                } else {
                    piece.segment.end - 1 - delta
                };
                return PortalPoint {
                    edge: piece.segment.edge,
                    normalized_offset,
                };
            }
            remaining -= piece.physical_weight;
        }
        unreachable!("compiled portal contains at least one weighted piece")
    }

    #[must_use]
    pub fn parameter_for_point(&self, point: PortalPoint) -> Option<u16> {
        let mut preceding = 0_u128;
        for piece in &self.pieces {
            if piece.segment.edge == point.edge
                && piece.segment.start <= point.normalized_offset
                && point.normalized_offset < piece.segment.end
            {
                let delta = if piece.clockwise_forward {
                    point.normalized_offset - piece.segment.start
                } else {
                    piece.segment.end - 1 - point.normalized_offset
                };
                let position =
                    preceding + u128::from(delta) * u128::from(piece.physical_extent_millimeters);
                let normalized = position * PARAMETER_SCALE / self.total_physical_weight;
                return Some(
                    u16::try_from(normalized.min(u128::from(u16::MAX))).unwrap_or(u16::MAX),
                );
            }
            preceding += piece.physical_weight;
        }
        None
    }
}

fn validate_portal_overlaps(boundaries: &[LogicalBoundary]) -> io::Result<()> {
    for (index, boundary) in boundaries.iter().enumerate() {
        for other in boundaries.iter().skip(index + 1) {
            if portals_overlap(&boundary.source, &other.source) {
                return Err(invalid_data("logical boundary source portals overlap"));
            }
            if boundary
                .peer_fingerprint
                .eq_ignore_ascii_case(&other.peer_fingerprint)
                && portals_overlap(&boundary.target, &other.target)
            {
                return Err(invalid_data(
                    "logical boundary target portals overlap on one peer",
                ));
            }
        }
    }
    Ok(())
}

fn portals_overlap(first: &BoundaryPortal, second: &BoundaryPortal) -> bool {
    first.segments.iter().any(|left| {
        second.segments.iter().any(|right| {
            left.edge == right.edge && left.start < right.end && left.end > right.start
        })
    })
}

fn ordered_segments(portal: &BoundaryPortal) -> io::Result<Vec<OrderedSegment>> {
    if portal.segments.is_empty() || portal.segments.len() > MAX_PORTAL_SEGMENTS {
        return Err(invalid_data(
            "boundary portal must contain between one and five segments",
        ));
    }
    let mut ordered = portal
        .segments
        .iter()
        .copied()
        .map(|segment| {
            if segment.start >= segment.end || segment.end > NORMALIZED_PORTAL_LENGTH {
                return Err(invalid_data("boundary portal segment interval is invalid"));
            }
            let (perimeter_start, perimeter_end, clockwise_forward) = perimeter_interval(segment);
            Ok(OrderedSegment {
                segment,
                perimeter_start,
                perimeter_end,
                clockwise_forward,
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    ordered.sort_by_key(|segment| (segment.perimeter_start, segment.perimeter_end));
    for pair in ordered.windows(2) {
        if pair[0].perimeter_end > pair[1].perimeter_start {
            return Err(invalid_data("boundary portal segments overlap"));
        }
    }

    let mut positive_gaps = Vec::new();
    for index in 0..ordered.len() {
        let next_start = if index + 1 == ordered.len() {
            ordered[0].perimeter_start + NORMALIZED_PERIMETER_LENGTH
        } else {
            ordered[index + 1].perimeter_start
        };
        let gap = next_start - ordered[index].perimeter_end;
        if gap > 0 {
            positive_gaps.push((gap, index));
        }
    }
    if positive_gaps.is_empty() {
        return Err(invalid_data(
            "boundary portal cannot consume the complete screen perimeter",
        ));
    }
    if positive_gaps.len() != 1 {
        return Err(invalid_data(
            "boundary portal segments do not form one continuous perimeter arc",
        ));
    }
    let cut = (positive_gaps[0].1 + 1) % ordered.len();
    ordered.rotate_left(cut);
    Ok(ordered)
}

const fn perimeter_interval(segment: BoundaryArcSegment) -> (u64, u64, bool) {
    let unit = NORMALIZED_PORTAL_LENGTH as u64;
    let start = segment.start as u64;
    let end = segment.end as u64;
    match segment.edge {
        ScreenBoundary::Top => (start, end, true),
        ScreenBoundary::Right => (unit + start, unit + end, true),
        ScreenBoundary::Bottom => (2 * unit + unit - end, 2 * unit + unit - start, false),
        ScreenBoundary::Left => (3 * unit + unit - end, 3 * unit + unit - start, false),
    }
}

const fn edge_pixel_extent(edge: ScreenBoundary, screen: PhysicalScreenSize) -> u32 {
    match edge {
        ScreenBoundary::Top | ScreenBoundary::Bottom => screen.width,
        ScreenBoundary::Left | ScreenBoundary::Right => screen.height,
    }
}

fn normalized_safe_range(extent: u32, guard: u32) -> (u32, u32) {
    let start =
        u32::try_from(u64::from(guard) * u64::from(NORMALIZED_PORTAL_LENGTH) / u64::from(extent))
            .expect("normalized guard start fits in u32");
    let end = u32::try_from(
        u64::from(extent - guard) * u64::from(NORMALIZED_PORTAL_LENGTH) / u64::from(extent),
    )
    .expect("normalized guard end fits in u32");
    (start, end)
}

fn canonical_fingerprint(value: &str) -> io::Result<String> {
    CertificateFingerprint::from_str(value).map(|fingerprint| fingerprint.to_string())
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

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint(byte: u8) -> String {
        CertificateFingerprint::from_bytes([byte; 32]).to_string()
    }

    fn screen(byte: u8, pixel: (u32, u32), physical: (u32, u32)) -> BoundaryScreen {
        BoundaryScreen {
            device_fingerprint: fingerprint(byte),
            pixel_width: pixel.0,
            pixel_height: pixel.1,
            physical_width_millimeters: physical.0,
            physical_height_millimeters: physical.1,
            rotation_quarter_turns: 0,
            canvas_x_millimeters: i32::from(byte) * 100,
            canvas_y_millimeters: 0,
        }
    }

    const fn segment(edge: ScreenBoundary, start: u32, end: u32) -> BoundaryArcSegment {
        BoundaryArcSegment { edge, start, end }
    }

    fn portal(segments: Vec<BoundaryArcSegment>) -> BoundaryPortal {
        BoundaryPortal { segments }
    }

    fn boundary(id: &str, peer: u8) -> LogicalBoundary {
        LogicalBoundary {
            id: id.to_owned(),
            peer_fingerprint: fingerprint(peer),
            mapping_revision: 4,
            local_confirmed_revision: 4,
            peer_confirmed_revision: 4,
            reversed: false,
            source: portal(vec![segment(ScreenBoundary::Top, 8_000, 28_000)]),
            target: portal(vec![segment(ScreenBoundary::Bottom, 20_000, 52_000)]),
        }
    }

    fn layout(boundaries: Vec<LogicalBoundary>) -> BoundaryLayoutV3 {
        BoundaryLayoutV3 {
            schema_version: LOGICAL_BOUNDARY_SCHEMA_VERSION,
            revision: 9,
            local_fingerprint: fingerprint(1),
            screens: vec![
                screen(1, (3_072, 1_728), (697, 392)),
                screen(2, (2_560, 1_440), (344, 194)),
                screen(3, (1_920, 1_080), (598, 336)),
            ],
            boundaries,
        }
    }

    #[test]
    fn strict_schema_round_trips_screen_and_paired_portals() {
        let model = layout(vec![boundary("pair-c", 2)]);
        model.validate_schema().unwrap();
        let json = serde_json::to_vec(&model).unwrap();
        assert_eq!(
            serde_json::from_slice::<BoundaryLayoutV3>(&json).unwrap(),
            model
        );

        let mut value = serde_json::to_value(&model).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("unknown".to_owned(), serde_json::Value::Bool(true));
        assert!(serde_json::from_value::<BoundaryLayoutV3>(value).is_err());
    }

    #[test]
    fn settings_generated_paired_portal_matches_the_v3_contract() {
        let json = format!(
            r#"{{
                "schemaVersion":3,
                "revision":1,
                "localFingerprint":"{}",
                "screens":[
                    {{"deviceFingerprint":"{}","pixelWidth":1920,"pixelHeight":1080,"physicalWidthMillimeters":598,"physicalHeightMillimeters":336,"rotationQuarterTurns":0,"canvasXMillimeters":0,"canvasYMillimeters":0}},
                    {{"deviceFingerprint":"{}","pixelWidth":2560,"pixelHeight":1440,"physicalWidthMillimeters":344,"physicalHeightMillimeters":194,"rotationQuarterTurns":0,"canvasXMillimeters":690,"canvasYMillimeters":150}}
                ],
                "boundaries":[{{
                    "id":"auto-peer-1","peerFingerprint":"{}","mappingRevision":1,
                    "localConfirmedRevision":1,"peerConfirmedRevision":0,"reversed":false,
                    "source":{{"segments":[{{"edge":"right","start":29193,"end":65536}}]}},
                    "target":{{"segments":[{{"edge":"left","start":0,"end":62724}}]}}
                }}]
            }}"#,
            fingerprint(1),
            fingerprint(1),
            fingerprint(2),
            fingerprint(2),
        );
        let model = serde_json::from_str::<BoundaryLayoutV3>(&json).unwrap();
        model.validate_schema().unwrap();
        assert!(model.compile(32, 64).is_err());
    }

    #[test]
    fn continuous_cross_corner_portal_needs_no_corner_group() {
        let mut pair = boundary("cross-corner", 2);
        pair.source = portal(vec![
            segment(ScreenBoundary::Top, 48_000, NORMALIZED_PORTAL_LENGTH),
            segment(ScreenBoundary::Right, 0, 18_000),
        ]);
        layout(vec![pair]).validate_schema().unwrap();
    }

    #[test]
    fn disconnected_and_full_perimeter_portals_are_rejected() {
        let mut disconnected = boundary("disconnected", 2);
        disconnected.source = portal(vec![
            segment(ScreenBoundary::Top, 2_000, 8_000),
            segment(ScreenBoundary::Right, 3_000, 9_000),
        ]);
        assert!(layout(vec![disconnected]).validate_schema().is_err());

        let mut full = boundary("full", 2);
        full.source = portal(vec![
            segment(ScreenBoundary::Top, 0, NORMALIZED_PORTAL_LENGTH),
            segment(ScreenBoundary::Right, 0, NORMALIZED_PORTAL_LENGTH),
            segment(ScreenBoundary::Bottom, 0, NORMALIZED_PORTAL_LENGTH),
            segment(ScreenBoundary::Left, 0, NORMALIZED_PORTAL_LENGTH),
        ]);
        assert!(layout(vec![full]).validate_schema().is_err());
    }

    #[test]
    fn without_peers_drops_only_the_switched_off_pair() {
        let mut pair_c = boundary("pair-c", 3);
        pair_c.source = portal(vec![segment(ScreenBoundary::Right, 8_000, 28_000)]);
        let model = layout(vec![boundary("pair-b", 2), pair_c]);
        let compiled = model.compile(32, 16).unwrap();
        let removed = HashSet::from([[2_u8; 32]]);
        let remaining = compiled.without_peers(&removed);
        assert_eq!(remaining.revision(), compiled.revision());
        assert_eq!(remaining.boundaries().len(), 1);
        assert_eq!(remaining.boundaries()[0].boundary_id(), "pair-c");
        assert_eq!(
            remaining.boundaries()[0].source().segments(),
            compiled.boundary("pair-c").unwrap().source().segments()
        );
        assert_eq!(
            compiled.without_peers(&HashSet::new()).boundaries().len(),
            2
        );
    }

    #[test]
    fn unconfirmed_revision_cannot_compile() {
        let mut pair = boundary("pending", 2);
        pair.peer_confirmed_revision = 3;
        let model = layout(vec![pair]);
        model.validate_schema().unwrap();
        assert!(model.compile(32, 16).is_err());
    }

    #[test]
    fn reversed_mapping_uses_cumulative_physical_length() {
        let mut pair = boundary("reversed", 2);
        pair.reversed = true;
        pair.source = portal(vec![segment(
            ScreenBoundary::Top,
            0,
            NORMALIZED_PORTAL_LENGTH,
        )]);
        pair.target = portal(vec![segment(
            ScreenBoundary::Right,
            0,
            NORMALIZED_PORTAL_LENGTH,
        )]);
        let compiled = layout(vec![pair]).compile(0, 16).unwrap();
        let boundary = compiled.boundary("reversed").unwrap();
        let transfer = boundary
            .source_to_target(PortalPoint {
                edge: ScreenBoundary::Top,
                normalized_offset: NORMALIZED_PORTAL_LENGTH / 4,
            })
            .unwrap();
        assert!((16_380..=16_385).contains(&transfer.parameter));
        assert_eq!(transfer.destination.edge, ScreenBoundary::Right);
        assert!((49_145..=49_155).contains(&transfer.destination.normalized_offset));

        let returned = boundary.target_to_source(transfer.destination).unwrap();
        assert_eq!(returned.destination.edge, ScreenBoundary::Top);
        assert!((16_375..=16_390).contains(&returned.destination.normalized_offset));
    }

    #[test]
    fn moving_source_does_not_mutate_target_portal() {
        let mut model = layout(vec![boundary("stable-target", 2)]);
        let target_before = model.boundaries[0].target.clone();
        model.boundaries[0].source = portal(vec![segment(ScreenBoundary::Left, 12_000, 42_000)]);
        model.boundaries[0].mapping_revision += 1;
        model.boundaries[0].local_confirmed_revision += 1;
        model.validate_schema().unwrap();
        assert_eq!(model.boundaries[0].target, target_before);
        assert!(!model.boundaries[0].is_confirmed());
        assert!(model.compile(32, 16).is_err());

        model.boundaries[0].peer_confirmed_revision += 1;
        assert!(model.boundaries[0].is_confirmed());
        model.compile(32, 16).unwrap();
    }

    #[test]
    fn final_compile_adds_guards_without_splitting_boundary_identity() {
        let mut pair = boundary("guarded-corner", 2);
        pair.source = portal(vec![
            segment(ScreenBoundary::Top, 46_000, NORMALIZED_PORTAL_LENGTH),
            segment(ScreenBoundary::Right, 0, 24_000),
        ]);
        pair.target = portal(vec![
            segment(ScreenBoundary::Bottom, 0, 20_000),
            segment(ScreenBoundary::Left, 42_000, NORMALIZED_PORTAL_LENGTH),
        ]);
        let compiled = layout(vec![pair]).compile(32, 16).unwrap();
        let boundary = compiled.boundary("guarded-corner").unwrap();
        assert_eq!(boundary.boundary_id(), "guarded-corner");
        assert_eq!(boundary.source().segments().len(), 2);
        assert!(boundary.source().segments()[0].end < NORMALIZED_PORTAL_LENGTH);
        assert!(boundary.source().segments()[1].start > 0);
        assert_eq!(boundary.target().segments().len(), 2);

        for parameter in [0, u16::MAX / 2, u16::MAX] {
            let source = boundary.source().point_at_parameter(parameter);
            let target = boundary.source_to_target(source).unwrap().destination;
            assert!(boundary.target().parameter_for_point(target).is_some());
        }
    }

    #[test]
    fn source_and_same_peer_target_overlaps_are_rejected() {
        let first = boundary("first", 2);
        let mut source_overlap = boundary("source-overlap", 3);
        source_overlap.source = portal(vec![segment(ScreenBoundary::Top, 27_000, 40_000)]);
        assert!(
            layout(vec![first.clone(), source_overlap])
                .validate_schema()
                .is_err()
        );

        let mut target_overlap = boundary("target-overlap", 2);
        target_overlap.source = portal(vec![segment(ScreenBoundary::Left, 5_000, 18_000)]);
        target_overlap.target = portal(vec![segment(ScreenBoundary::Bottom, 30_000, 60_000)]);
        assert!(
            layout(vec![first, target_overlap])
                .validate_schema()
                .is_err()
        );
    }

    #[test]
    fn identical_target_ranges_on_different_peers_are_allowed() {
        let first = boundary("first", 2);
        let mut second = boundary("second", 3);
        second.source = portal(vec![segment(ScreenBoundary::Left, 5_000, 18_000)]);
        second.target = first.target.clone();
        layout(vec![first, second]).validate_schema().unwrap();
    }

    #[test]
    fn quarter_turn_swaps_pixel_and_physical_extents() {
        let mut rotated = screen(1, (2_560, 1_440), (600, 340));
        rotated.rotation_quarter_turns = 1;
        assert_eq!(
            rotated.oriented_pixel_size().unwrap(),
            PhysicalScreenSize::new(1_440, 2_560).unwrap()
        );
        assert_eq!(rotated.physical_extent(ScreenBoundary::Top), 340);
        assert_eq!(rotated.physical_extent(ScreenBoundary::Right), 600);
    }
}
