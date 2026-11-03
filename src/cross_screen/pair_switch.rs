//! Per-pair cross-screen switches: one record per paired computer, mirrored on both ends.
//!
//! The switch is separate from the logical boundary layout, so turning one pair off never edits
//! boundary geometry or the apply gate. A missing record means the pair is enabled, which keeps
//! existing installations unchanged without a migration.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr as _;

use serde::{Deserialize, Serialize};

use crate::device_store::DeviceStore;
use crate::security::CertificateFingerprint;
use crate::workspace::DeviceId;

const FILE: &str = "cross_screen.pair_switch.v1.json";
const NEXT_FILE: &str = "cross_screen.pair_switch.v1.next";
const SCHEMA_VERSION: u16 = 1;
const FILE_LIMIT: u64 = 16 * 1024;
const MAX_PAIRS: usize = 4;

/// The state of one pair. `revision` orders changes; the reserved maximum is never issued.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PairSwitchRecord {
    pub enabled: bool,
    pub revision: u64,
}

impl Default for PairSwitchRecord {
    fn default() -> Self {
        Self {
            enabled: true,
            revision: 0,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PairSwitches {
    schema: u16,
    pairs: BTreeMap<String, PairSwitchRecord>,
}

impl Default for PairSwitches {
    fn default() -> Self {
        Self {
            schema: SCHEMA_VERSION,
            pairs: BTreeMap::new(),
        }
    }
}

impl PairSwitches {
    /// Loads the mirrored switch file, treating a missing file as "every pair enabled".
    ///
    /// # Errors
    /// Reports a corrupt, oversized, or unsupported file instead of silently resetting it.
    pub fn load_or_default(store: &DeviceStore) -> io::Result<Self> {
        let file = match OpenOptions::new().read(true).open(file_path(store)) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error),
        };
        let mut text = String::new();
        file.take(FILE_LIMIT + 1).read_to_string(&mut text)?;
        if text.len() as u64 > FILE_LIMIT {
            return Err(invalid_data("pair switch file is too large"));
        }
        let switches: Self =
            serde_json::from_str(&text).map_err(|error| invalid_data(error.to_string()))?;
        switches.validate()?;
        Ok(switches)
    }

    /// Writes the file durably: a temporary file is fully written before it replaces the old one.
    ///
    /// # Errors
    /// Returns an error for invalid contents or a failed write.
    pub fn save(&self, store: &DeviceStore) -> io::Result<()> {
        self.validate()?;
        fs::create_dir_all(store.root())?;
        let bytes = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        let path = file_path(store);
        let temporary = store.root().join(NEXT_FILE);
        write_synced(&temporary, &bytes)?;
        replace_file(&temporary, &path)
    }

    #[must_use]
    pub fn record(&self, peer: DeviceId) -> PairSwitchRecord {
        self.pairs.get(&key(peer)).copied().unwrap_or_default()
    }

    #[must_use]
    pub fn is_enabled(&self, peer: DeviceId) -> bool {
        self.record(peer).enabled
    }

    /// Every peer whose pair is currently switched off.
    #[must_use]
    pub fn disabled_peers(&self) -> BTreeSet<DeviceId> {
        self.pairs
            .iter()
            .filter(|(_, record)| !record.enabled)
            .filter_map(|(peer, _)| CertificateFingerprint::from_str(peer).ok())
            .map(CertificateFingerprint::bytes)
            .collect()
    }

    /// Applies a change whose `expected_revision` matches the stored record.
    ///
    /// Setting the value a pair already has is a no-op and does not advance the revision.
    ///
    /// # Errors
    /// Rejects a stale `expected_revision`, an exhausted revision counter, or too many pairs.
    pub fn set_enabled(
        &mut self,
        peer: DeviceId,
        enabled: bool,
        expected_revision: u64,
    ) -> io::Result<u64> {
        let current = self.record(peer);
        if current.revision != expected_revision {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pair switch is based on a stale revision",
            ));
        }
        if current.enabled == enabled {
            return Ok(current.revision);
        }
        let revision = current
            .revision
            .checked_add(1)
            .filter(|revision| *revision != u64::MAX)
            .ok_or_else(|| invalid_data("pair switch revision is exhausted"))?;
        self.pairs
            .insert(key(peer), PairSwitchRecord { enabled, revision });
        self.validate()?;
        Ok(revision)
    }

    /// Adopts a peer's record only when it is strictly newer, so repeated alignment is idempotent.
    ///
    /// # Errors
    /// Rejects a record that would exceed the pair limit or use the reserved revision.
    pub fn adopt_if_newer(&mut self, peer: DeviceId, record: PairSwitchRecord) -> io::Result<bool> {
        if record.revision == u64::MAX || record.revision <= self.record(peer).revision {
            return Ok(false);
        }
        self.pairs.insert(key(peer), record);
        self.validate()?;
        Ok(true)
    }

    fn validate(&self) -> io::Result<()> {
        if self.schema != SCHEMA_VERSION {
            return Err(invalid_data("unsupported pair switch schema version"));
        }
        if self.pairs.len() > MAX_PAIRS {
            return Err(invalid_data("pair switch file contains too many pairs"));
        }
        for (peer, record) in &self.pairs {
            CertificateFingerprint::from_str(peer)
                .map_err(|_| invalid_data("pair switch peer fingerprint is invalid"))?;
            if record.revision == u64::MAX {
                return Err(invalid_data(
                    "pair switch revision uses the reserved maximum",
                ));
            }
        }
        Ok(())
    }
}

/// Messages on the `ApplicationProtocol::PairSwitch` queue of an authenticated link.
///
/// A request asks the peer to store `enabled` for this pair, but only when the peer's own record
/// still has `expected_revision`. The acknowledgement carries the peer's resulting record, which the
/// requester adopts. `Status` is sent periodically so that a missed acknowledgement converges: the
/// strictly newer record always wins on both ends.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum PairSwitchMessage {
    Request {
        request_id: u64,
        enabled: bool,
        expected_revision: u64,
    },
    Ack {
        request_id: u64,
        accepted: bool,
        record: PairSwitchRecord,
    },
    Status {
        record: PairSwitchRecord,
    },
}

const MESSAGE_LIMIT: usize = 512;

/// # Errors
/// Returns an error when the message cannot be encoded within its bound.
pub fn encode_message(message: &PairSwitchMessage) -> io::Result<Vec<u8>> {
    let bytes = serde_json::to_vec(message).map_err(io::Error::other)?;
    if bytes.is_empty() || bytes.len() > MESSAGE_LIMIT {
        return Err(invalid_data("pair switch message exceeds its size bound"));
    }
    Ok(bytes)
}

/// # Errors
/// Returns an error for oversized, malformed, or unknown messages.
pub fn decode_message(bytes: &[u8]) -> io::Result<PairSwitchMessage> {
    if bytes.len() > MESSAGE_LIMIT {
        return Err(invalid_data("pair switch message exceeds its size bound"));
    }
    serde_json::from_slice(bytes).map_err(|error| invalid_data(error.to_string()))
}

fn key(peer: DeviceId) -> String {
    CertificateFingerprint::from_bytes(peer).to_string()
}

fn file_path(store: &DeviceStore) -> PathBuf {
    store.root().join(FILE)
}

fn write_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn replace_file(temporary: &Path, path: &Path) -> io::Result<()> {
    // std::fs::rename replaces an existing destination on Windows, so the old file is never removed
    // before the new content is complete.
    fs::rename(temporary, path)
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(byte: u8) -> DeviceId {
        [byte; 32]
    }

    #[test]
    fn missing_pairs_are_enabled_at_revision_zero() {
        let switches = PairSwitches::default();
        assert!(switches.is_enabled(peer(1)));
        assert_eq!(switches.record(peer(1)), PairSwitchRecord::default());
        assert!(switches.disabled_peers().is_empty());
    }

    #[test]
    fn set_enabled_advances_revision_only_on_change() {
        let mut switches = PairSwitches::default();
        assert_eq!(switches.set_enabled(peer(1), false, 0).unwrap(), 1);
        assert!(!switches.is_enabled(peer(1)));
        assert_eq!(switches.set_enabled(peer(1), false, 1).unwrap(), 1);
        assert_eq!(switches.set_enabled(peer(1), true, 1).unwrap(), 2);
        assert!(switches.is_enabled(peer(1)));
    }

    #[test]
    fn stale_revision_is_rejected_without_changing_state() {
        let mut switches = PairSwitches::default();
        switches.set_enabled(peer(2), false, 0).unwrap();
        assert!(switches.set_enabled(peer(2), true, 0).is_err());
        assert!(!switches.is_enabled(peer(2)));
        assert_eq!(switches.record(peer(2)).revision, 1);
    }

    #[test]
    fn disabling_one_pair_leaves_other_pairs_enabled() {
        let mut switches = PairSwitches::default();
        switches.set_enabled(peer(1), false, 0).unwrap();
        assert_eq!(switches.disabled_peers(), BTreeSet::from([peer(1)]));
        assert!(switches.is_enabled(peer(3)));
    }

    #[test]
    fn adopt_only_takes_strictly_newer_records() {
        let mut switches = PairSwitches::default();
        switches.set_enabled(peer(1), false, 0).unwrap();
        let older = PairSwitchRecord {
            enabled: true,
            revision: 1,
        };
        let newer = PairSwitchRecord {
            enabled: true,
            revision: 2,
        };
        assert!(!switches.adopt_if_newer(peer(1), older).unwrap());
        assert!(!switches.is_enabled(peer(1)));
        assert!(switches.adopt_if_newer(peer(1), newer).unwrap());
        assert!(switches.is_enabled(peer(1)));
        assert!(!switches.adopt_if_newer(peer(1), newer).unwrap());
    }

    #[test]
    fn validation_rejects_bad_schema_limits_and_reserved_revision() {
        let mut switches = PairSwitches::default();
        switches.schema = 2;
        assert!(switches.validate().is_err());

        let mut switches = PairSwitches::default();
        for byte in 1..=5 {
            switches
                .pairs
                .insert(key(peer(byte)), PairSwitchRecord::default());
        }
        assert!(switches.validate().is_err());

        let mut switches = PairSwitches::default();
        switches.pairs.insert(
            key(peer(1)),
            PairSwitchRecord {
                enabled: false,
                revision: u64::MAX,
            },
        );
        assert!(switches.validate().is_err());
    }

    #[test]
    fn protocol_messages_round_trip_and_reject_unknown_or_oversized_input() {
        let messages = [
            PairSwitchMessage::Request {
                request_id: 7,
                enabled: false,
                expected_revision: 3,
            },
            PairSwitchMessage::Ack {
                request_id: 7,
                accepted: true,
                record: PairSwitchRecord {
                    enabled: false,
                    revision: 4,
                },
            },
            PairSwitchMessage::Status {
                record: PairSwitchRecord::default(),
            },
        ];
        for message in messages {
            let bytes = encode_message(&message).unwrap();
            assert_eq!(decode_message(&bytes).unwrap(), message);
        }
        assert!(
            decode_message(br#"{"kind":"status","record":{"enabled":true,"revision":1},"x":1}"#)
                .is_err()
        );
        assert!(decode_message(br#"{"kind":"unknown"}"#).is_err());
        assert!(decode_message(&vec![b' '; MESSAGE_LIMIT + 1]).is_err());
    }

    #[test]
    fn json_round_trip_and_unknown_fields_are_rejected() {
        let mut switches = PairSwitches::default();
        switches.set_enabled(peer(1), false, 0).unwrap();
        let text = serde_json::to_string(&switches).unwrap();
        let decoded: PairSwitches = serde_json::from_str(&text).unwrap();
        assert_eq!(decoded, switches);
        assert!(
            serde_json::from_str::<PairSwitches>(r#"{"schema":1,"pairs":{},"extra":true}"#)
                .is_err()
        );
    }
}
