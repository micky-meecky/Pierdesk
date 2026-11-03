use std::io;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use super::logical_boundary::{BoundaryLayoutV3, BoundaryScreen, LogicalBoundary};
use crate::security::CertificateFingerprint;

const BOUNDARY_SYNC_SCHEMA_VERSION: u16 = 3;
const BOUNDARY_SYNC_PAYLOAD_LIMIT: usize = 60 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundarySyncPhase {
    Preview,
    Applied,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundarySyncSurface {
    Spatial,
    Boundary,
    Complete,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RemoteBoundaryWorkspace {
    pub sequence: u64,
    pub phase: BoundarySyncPhase,
    pub surface: BoundarySyncSurface,
    pub initiator_fingerprint: String,
    pub layout: BoundaryLayoutV3,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct BoundarySyncEnvelope {
    schema_version: u16,
    phase: BoundarySyncPhase,
    surface: BoundarySyncSurface,
    initiator_fingerprint: String,
    layout: BoundaryLayoutV3,
}

impl BoundarySyncEnvelope {
    fn validate_source(
        &mut self,
        expected_author: CertificateFingerprint,
        receiver: CertificateFingerprint,
    ) -> io::Result<()> {
        if self.schema_version != BOUNDARY_SYNC_SCHEMA_VERSION {
            return Err(invalid_data("unsupported boundary sync schema version"));
        }
        if matches!(self.phase, BoundarySyncPhase::Applied)
            != matches!(self.surface, BoundarySyncSurface::Complete)
        {
            return Err(invalid_data(
                "applied boundary sync must target the completion surface",
            ));
        }
        self.layout.canonicalize_fingerprints()?;
        self.layout.validate_schema()?;
        self.initiator_fingerprint =
            CertificateFingerprint::from_str(&self.initiator_fingerprint)?.to_string();
        if self.layout.local_fingerprint != expected_author.to_string() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "boundary preview author does not match the authenticated peer",
            ));
        }
        let receiver = receiver.to_string();
        if self.initiator_fingerprint != self.layout.local_fingerprint
            && self.initiator_fingerprint != receiver
        {
            return Err(invalid_data(
                "boundary preview initiator must belong to the authenticated device pair",
            ));
        }
        if self.layout.screens.len() != 2
            || !self
                .layout
                .screens
                .iter()
                .any(|screen| screen.device_fingerprint == receiver)
            || self
                .layout
                .boundaries
                .iter()
                .any(|boundary| boundary.peer_fingerprint != receiver)
        {
            return Err(invalid_data(
                "boundary preview must contain exactly the authenticated device pair",
            ));
        }
        Ok(())
    }
}

/// Encodes the part of a local workspace that belongs to one authenticated peer.
///
/// # Errors
///
/// Returns an error when the workspace is invalid or the bounded payload cannot be encoded.
pub fn encode_boundary_sync(
    layout: &BoundaryLayoutV3,
    peer: CertificateFingerprint,
    phase: BoundarySyncPhase,
    surface: BoundarySyncSurface,
) -> io::Result<Option<Vec<u8>>> {
    let initiator = CertificateFingerprint::from_str(&layout.local_fingerprint)?;
    encode_boundary_sync_with_initiator(layout, peer, phase, surface, initiator)
}

/// Encodes one authenticated device pair while preserving the device that initiated the edit.
///
/// The initiator may differ from the sender when a receiver relays confirmation for an edit that
/// originated on its peer. This lets every UI follow the operator without making the operator's
/// own page jump when its change is echoed back.
///
/// # Errors
///
/// Returns an error when the workspace, peer, or initiator is invalid.
pub fn encode_boundary_sync_with_initiator(
    layout: &BoundaryLayoutV3,
    peer: CertificateFingerprint,
    phase: BoundarySyncPhase,
    surface: BoundarySyncSurface,
    initiator: CertificateFingerprint,
) -> io::Result<Option<Vec<u8>>> {
    if matches!(phase, BoundarySyncPhase::Applied)
        != matches!(surface, BoundarySyncSurface::Complete)
    {
        return Err(invalid_data(
            "applied boundary sync must target the completion surface",
        ));
    }
    let mut source = layout.clone();
    source.canonicalize_fingerprints()?;
    source.validate_schema()?;
    let peer = peer.to_string();
    let Some(local_screen) = source
        .screens
        .iter()
        .find(|screen| screen.device_fingerprint == source.local_fingerprint)
        .cloned()
    else {
        return Err(invalid_data(
            "boundary workspace is missing its local screen",
        ));
    };
    let Some(peer_screen) = source
        .screens
        .iter()
        .find(|screen| screen.device_fingerprint == peer)
        .cloned()
    else {
        return Ok(None);
    };
    let pair = BoundaryLayoutV3 {
        schema_version: source.schema_version,
        revision: source.revision,
        local_fingerprint: source.local_fingerprint,
        screens: vec![local_screen, peer_screen],
        boundaries: source
            .boundaries
            .into_iter()
            .filter(|boundary| boundary.peer_fingerprint == peer)
            .collect(),
    };
    pair.validate_schema()?;
    let payload = serde_json::to_vec(&BoundarySyncEnvelope {
        schema_version: BOUNDARY_SYNC_SCHEMA_VERSION,
        phase,
        surface,
        initiator_fingerprint: initiator.to_string(),
        layout: pair,
    })
    .map_err(io::Error::other)?;
    if payload.is_empty() || payload.len() > BOUNDARY_SYNC_PAYLOAD_LIMIT {
        return Err(invalid_data(
            "boundary sync payload exceeds its fixed limit",
        ));
    }
    Ok(Some(payload))
}

/// Decodes and mirrors a peer-authored pair so the receiver is always the local device.
///
/// # Errors
///
/// Returns an error for malformed, oversized, unauthenticated, or invalid pair data.
pub fn decode_boundary_sync(
    payload: &[u8],
    expected_author: CertificateFingerprint,
    receiver: CertificateFingerprint,
    sequence: u64,
) -> io::Result<RemoteBoundaryWorkspace> {
    if payload.is_empty() || payload.len() > BOUNDARY_SYNC_PAYLOAD_LIMIT {
        return Err(invalid_data(
            "boundary sync payload is outside its fixed limit",
        ));
    }
    let mut envelope = serde_json::from_slice::<BoundarySyncEnvelope>(payload)
        .map_err(|error| invalid_data(format!("invalid boundary sync payload: {error}")))?;
    envelope.validate_source(expected_author, receiver)?;
    let layout = mirror_pair_layout(&envelope.layout, receiver)?;
    Ok(RemoteBoundaryWorkspace {
        sequence,
        phase: envelope.phase,
        surface: envelope.surface,
        initiator_fingerprint: envelope.initiator_fingerprint,
        layout,
    })
}

/// Merges a receiver-owned peer preview into the local editable workspace.
///
/// Existing screens and boundaries for unrelated peers are retained. The incoming pair is moved
/// by one common offset so the current local-screen origin remains stable.
///
/// # Errors
///
/// Returns an error when either layout is invalid or belongs to a different local device.
pub fn accept_remote_preview(
    current: &BoundaryLayoutV3,
    remote: &RemoteBoundaryWorkspace,
) -> io::Result<BoundaryLayoutV3> {
    let mut current = current.clone();
    current.canonicalize_fingerprints()?;
    current.validate_schema()?;
    let mut incoming = remote.layout.clone();
    incoming.canonicalize_fingerprints()?;
    incoming.validate_schema()?;
    if incoming.local_fingerprint != current.local_fingerprint {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "remote boundary preview belongs to another local device",
        ));
    }
    let peer = incoming_pair_peer(&incoming)?;
    let current_local = screen(&current, &current.local_fingerprint)?.clone();
    let incoming_local = screen(&incoming, &incoming.local_fingerprint)?;
    let offset_x = i64::from(current_local.canvas_x_millimeters)
        - i64::from(incoming_local.canvas_x_millimeters);
    let offset_y = i64::from(current_local.canvas_y_millimeters)
        - i64::from(incoming_local.canvas_y_millimeters);
    let mut incoming_peer = screen(&incoming, &peer)?.clone();
    incoming_peer.canvas_x_millimeters = shifted_coordinate(
        incoming_peer.canvas_x_millimeters,
        offset_x,
        "remote screen x position",
    )?;
    incoming_peer.canvas_y_millimeters = shifted_coordinate(
        incoming_peer.canvas_y_millimeters,
        offset_y,
        "remote screen y position",
    )?;

    if let Some(existing) = current
        .screens
        .iter_mut()
        .find(|candidate| candidate.device_fingerprint == peer)
    {
        *existing = incoming_peer;
    } else {
        current.screens.push(incoming_peer);
    }
    current
        .boundaries
        .retain(|boundary| boundary.peer_fingerprint != peer);
    current
        .boundaries
        .extend(incoming.boundaries.into_iter().map(|mut boundary| {
            boundary.local_confirmed_revision = boundary.mapping_revision;
            boundary
        }));
    current.validate_schema()?;
    Ok(current)
}

/// Creates the receiver's first editable workspace from a mirrored peer preview.
///
/// # Errors
///
/// Returns an error when the mirrored layout is invalid.
pub fn initialize_from_remote_preview(
    remote: &RemoteBoundaryWorkspace,
) -> io::Result<BoundaryLayoutV3> {
    let mut layout = remote.layout.clone();
    layout.canonicalize_fingerprints()?;
    for boundary in &mut layout.boundaries {
        boundary.local_confirmed_revision = boundary.mapping_revision;
    }
    layout.validate_schema()?;
    Ok(layout)
}

/// Reports whether a peer preview describes the same pair geometry as the local draft.
///
/// # Errors
///
/// Returns an error when fingerprints or pair structure are invalid.
pub fn remote_pair_geometry_matches(
    current: &BoundaryLayoutV3,
    remote: &RemoteBoundaryWorkspace,
) -> io::Result<bool> {
    let mut current = current.clone();
    current.canonicalize_fingerprints()?;
    let mut incoming = remote.layout.clone();
    incoming.canonicalize_fingerprints()?;
    if current.local_fingerprint != incoming.local_fingerprint {
        return Ok(false);
    }
    let peer = incoming_pair_peer(&incoming)?;
    Ok(pair_geometry_matches(&current, &incoming, &peer))
}

/// Imports only peer-confirmation fields when the two devices already hold identical geometry.
///
/// # Errors
///
/// Returns an error when fingerprints or pair structure are invalid.
pub fn merge_matching_confirmations(
    current: &BoundaryLayoutV3,
    remote: &RemoteBoundaryWorkspace,
) -> io::Result<Option<BoundaryLayoutV3>> {
    let mut current = current.clone();
    current.canonicalize_fingerprints()?;
    let mut incoming = remote.layout.clone();
    incoming.canonicalize_fingerprints()?;
    if current.local_fingerprint != incoming.local_fingerprint {
        return Ok(None);
    }
    let peer = incoming_pair_peer(&incoming)?;
    if !pair_geometry_matches(&current, &incoming, &peer) {
        return Ok(None);
    }
    let mut changed = false;
    for boundary in current
        .boundaries
        .iter_mut()
        .filter(|boundary| boundary.peer_fingerprint == peer)
    {
        let Some(remote_boundary) = incoming
            .boundaries
            .iter()
            .find(|candidate| candidate.id == boundary.id)
        else {
            return Ok(None);
        };
        let confirmation = remote_boundary.peer_confirmed_revision;
        if confirmation == boundary.mapping_revision
            && boundary.peer_confirmed_revision != confirmation
        {
            boundary.peer_confirmed_revision = confirmation;
            changed = true;
        }
    }
    Ok(changed.then_some(current))
}

fn mirror_pair_layout(
    source: &BoundaryLayoutV3,
    receiver: CertificateFingerprint,
) -> io::Result<BoundaryLayoutV3> {
    let receiver = receiver.to_string();
    let author = source.local_fingerprint.clone();
    let receiver_screen = screen(source, &receiver)?.clone();
    let author_screen = screen(source, &author)?.clone();
    let origin_x = receiver_screen.canvas_x_millimeters;
    let origin_y = receiver_screen.canvas_y_millimeters;
    let normalize = |mut value: BoundaryScreen| -> io::Result<BoundaryScreen> {
        value.canvas_x_millimeters = shifted_coordinate(
            value.canvas_x_millimeters,
            -i64::from(origin_x),
            "mirrored screen x position",
        )?;
        value.canvas_y_millimeters = shifted_coordinate(
            value.canvas_y_millimeters,
            -i64::from(origin_y),
            "mirrored screen y position",
        )?;
        Ok(value)
    };
    let boundaries = source
        .boundaries
        .iter()
        .map(|boundary| LogicalBoundary {
            id: boundary.id.clone(),
            peer_fingerprint: author.clone(),
            mapping_revision: boundary.mapping_revision,
            local_confirmed_revision: boundary.peer_confirmed_revision,
            peer_confirmed_revision: boundary.local_confirmed_revision,
            reversed: boundary.reversed,
            source: boundary.target.clone(),
            target: boundary.source.clone(),
        })
        .collect();
    let mirrored = BoundaryLayoutV3 {
        schema_version: source.schema_version,
        revision: source.revision,
        local_fingerprint: receiver,
        screens: vec![normalize(receiver_screen)?, normalize(author_screen)?],
        boundaries,
    };
    mirrored.validate_schema()?;
    Ok(mirrored)
}

fn pair_geometry_matches(
    current: &BoundaryLayoutV3,
    incoming: &BoundaryLayoutV3,
    peer: &str,
) -> bool {
    let current_boundaries = current
        .boundaries
        .iter()
        .filter(|boundary| boundary.peer_fingerprint == peer)
        .collect::<Vec<_>>();
    let incoming_boundaries = incoming
        .boundaries
        .iter()
        .filter(|boundary| boundary.peer_fingerprint == peer)
        .collect::<Vec<_>>();
    if current_boundaries.len() != incoming_boundaries.len()
        || pair_screen_mapping_key(current, peer) != pair_screen_mapping_key(incoming, peer)
    {
        return false;
    }
    current_boundaries.iter().all(|boundary| {
        incoming_boundaries.iter().any(|candidate| {
            boundary.id == candidate.id
                && boundary.peer_fingerprint == candidate.peer_fingerprint
                && boundary.mapping_revision == candidate.mapping_revision
                && boundary.reversed == candidate.reversed
                && boundary.source == candidate.source
                && boundary.target == candidate.target
        })
    })
}

type ScreenMappingKey = (u32, u32, u32, u32, u8);
type PairScreenMappingKey = (ScreenMappingKey, ScreenMappingKey, i64, i64);

fn pair_screen_mapping_key(layout: &BoundaryLayoutV3, peer: &str) -> Option<PairScreenMappingKey> {
    let local = screen(layout, &layout.local_fingerprint).ok()?;
    let peer = screen(layout, peer).ok()?;
    Some((
        screen_mapping_key(Some(local))?,
        screen_mapping_key(Some(peer))?,
        i64::from(peer.canvas_x_millimeters) - i64::from(local.canvas_x_millimeters),
        i64::from(peer.canvas_y_millimeters) - i64::from(local.canvas_y_millimeters),
    ))
}

fn screen_mapping_key(screen: Option<&BoundaryScreen>) -> Option<ScreenMappingKey> {
    screen.map(|screen| {
        (
            screen.pixel_width,
            screen.pixel_height,
            screen.physical_width_millimeters,
            screen.physical_height_millimeters,
            screen.rotation_quarter_turns,
        )
    })
}

fn incoming_pair_peer(layout: &BoundaryLayoutV3) -> io::Result<String> {
    layout
        .screens
        .iter()
        .find(|screen| screen.device_fingerprint != layout.local_fingerprint)
        .map(|screen| screen.device_fingerprint.clone())
        .ok_or_else(|| invalid_data("remote boundary preview is missing its peer screen"))
}

fn screen<'a>(layout: &'a BoundaryLayoutV3, fingerprint: &str) -> io::Result<&'a BoundaryScreen> {
    layout
        .screens
        .iter()
        .find(|screen| screen.device_fingerprint == fingerprint)
        .ok_or_else(|| invalid_data("boundary pair is missing a screen"))
}

fn shifted_coordinate(value: i32, offset: i64, label: &str) -> io::Result<i32> {
    i32::try_from(i64::from(value).saturating_add(offset))
        .map_err(|_| invalid_data(format!("{label} is outside the supported range")))
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cross_screen::{BoundaryArcSegment, BoundaryPortal, ScreenBoundary};

    const LOCAL: &str = "00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:01";
    const PEER: &str = "00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:02";

    fn sample() -> BoundaryLayoutV3 {
        BoundaryLayoutV3 {
            schema_version: 3,
            revision: 7,
            local_fingerprint: LOCAL.to_owned(),
            screens: vec![
                BoundaryScreen {
                    device_fingerprint: LOCAL.to_owned(),
                    pixel_width: 1920,
                    pixel_height: 1080,
                    physical_width_millimeters: 598,
                    physical_height_millimeters: 336,
                    rotation_quarter_turns: 0,
                    canvas_x_millimeters: 100,
                    canvas_y_millimeters: 200,
                },
                BoundaryScreen {
                    device_fingerprint: PEER.to_owned(),
                    pixel_width: 2240,
                    pixel_height: 1400,
                    physical_width_millimeters: 302,
                    physical_height_millimeters: 188,
                    rotation_quarter_turns: 0,
                    canvas_x_millimeters: 150,
                    canvas_y_millimeters: -10,
                },
            ],
            boundaries: vec![LogicalBoundary {
                id: "pair-1".to_owned(),
                peer_fingerprint: PEER.to_owned(),
                mapping_revision: 2,
                local_confirmed_revision: 2,
                peer_confirmed_revision: 0,
                reversed: true,
                source: BoundaryPortal {
                    segments: vec![BoundaryArcSegment {
                        edge: ScreenBoundary::Top,
                        start: 100,
                        end: 20_000,
                    }],
                },
                target: BoundaryPortal {
                    segments: vec![BoundaryArcSegment {
                        edge: ScreenBoundary::Bottom,
                        start: 200,
                        end: 22_000,
                    }],
                },
            }],
        }
    }

    #[test]
    fn pair_round_trip_mirrors_screens_portals_and_confirmations() {
        let local = CertificateFingerprint::from_str(LOCAL).unwrap();
        let peer = CertificateFingerprint::from_str(PEER).unwrap();
        let payload = encode_boundary_sync(
            &sample(),
            peer,
            BoundarySyncPhase::Preview,
            BoundarySyncSurface::Boundary,
        )
        .unwrap()
        .unwrap();
        let mirrored = decode_boundary_sync(&payload, local, peer, 9).unwrap();
        assert_eq!(mirrored.sequence, 9);
        assert_eq!(mirrored.surface, BoundarySyncSurface::Boundary);
        assert_eq!(mirrored.initiator_fingerprint, LOCAL);
        assert_eq!(mirrored.layout.local_fingerprint, PEER);
        assert_eq!(mirrored.layout.screens[0].canvas_x_millimeters, 0);
        assert_eq!(mirrored.layout.screens[1].canvas_x_millimeters, -50);
        assert_eq!(mirrored.layout.screens[1].canvas_y_millimeters, 210);
        let boundary = &mirrored.layout.boundaries[0];
        assert_eq!(boundary.peer_fingerprint, LOCAL);
        assert_eq!(boundary.source, sample().boundaries[0].target);
        assert_eq!(boundary.target, sample().boundaries[0].source);
        assert_eq!(boundary.local_confirmed_revision, 0);
        assert_eq!(boundary.peer_confirmed_revision, 2);
    }

    #[test]
    fn synchronization_phase_requires_the_matching_product_surface() {
        let local = CertificateFingerprint::from_str(LOCAL).unwrap();
        let peer = CertificateFingerprint::from_str(PEER).unwrap();
        assert!(
            encode_boundary_sync_with_initiator(
                &sample(),
                peer,
                BoundarySyncPhase::Applied,
                BoundarySyncSurface::Spatial,
                local,
            )
            .is_err()
        );
        assert!(
            encode_boundary_sync_with_initiator(
                &sample(),
                peer,
                BoundarySyncPhase::Preview,
                BoundarySyncSurface::Complete,
                local,
            )
            .is_err()
        );
    }

    #[test]
    fn relayed_confirmation_preserves_the_original_edit_initiator() {
        let local = CertificateFingerprint::from_str(LOCAL).unwrap();
        let peer = CertificateFingerprint::from_str(PEER).unwrap();
        let first = encode_boundary_sync_with_initiator(
            &sample(),
            peer,
            BoundarySyncPhase::Preview,
            BoundarySyncSurface::Boundary,
            local,
        )
        .unwrap()
        .unwrap();
        let mirrored = decode_boundary_sync(&first, local, peer, 1).unwrap();
        let reply = encode_boundary_sync_with_initiator(
            &mirrored.layout,
            local,
            BoundarySyncPhase::Preview,
            BoundarySyncSurface::Boundary,
            local,
        )
        .unwrap()
        .unwrap();
        let returned = decode_boundary_sync(&reply, peer, local, 2).unwrap();
        assert_eq!(returned.initiator_fingerprint, LOCAL);
        assert_eq!(returned.layout.local_fingerprint, LOCAL);
    }

    #[test]
    fn accepting_preview_preserves_unrelated_workspace_and_confirms_local_side() {
        let local = CertificateFingerprint::from_str(LOCAL).unwrap();
        let peer = CertificateFingerprint::from_str(PEER).unwrap();
        let payload = encode_boundary_sync(
            &sample(),
            peer,
            BoundarySyncPhase::Preview,
            BoundarySyncSurface::Boundary,
        )
        .unwrap()
        .unwrap();
        let remote = decode_boundary_sync(&payload, local, peer, 1).unwrap();
        let mut current = remote.layout.clone();
        current.boundaries.clear();
        current.screens[0].canvas_x_millimeters = 500;
        current.screens[0].canvas_y_millimeters = 600;
        current.screens.remove(1);
        let accepted = accept_remote_preview(&current, &remote).unwrap();
        assert_eq!(accepted.screens.len(), 2);
        assert_eq!(accepted.screens[1].canvas_x_millimeters, 450);
        assert_eq!(accepted.screens[1].canvas_y_millimeters, 810);
        assert_eq!(accepted.boundaries[0].local_confirmed_revision, 2);
        assert_eq!(accepted.boundaries[0].peer_confirmed_revision, 2);
    }
}
