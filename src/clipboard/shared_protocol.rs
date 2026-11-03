use std::collections::HashSet;
use std::io;
use std::sync::Arc;

use sha2::{Digest as _, Sha256};

use super::snapshot::{
    ClipboardEventId, MAX_RICH_TEXT_BYTES, MAX_SHARED_FORMATS, MAX_SHARED_SNAPSHOT_BYTES,
    MAX_UNICODE_TEXT_BYTES, SharedClipboardSnapshot, SharedFormatData, SharedFormatKind,
    validate_format_set,
};

pub(crate) const SHARED_MAGIC: &[u8; 8] = b"CFSHARE3";
pub(crate) const SHARED_INSTALLED_ACK: &[u8; 8] = b"CFSHIN03";
pub(crate) const SHARED_READY_ACK: &[u8; 8] = b"CFSHRD03";
pub(crate) const SHARED_FAILED_ACK: &[u8; 8] = b"CFSHFL03";
pub(crate) const SHARED_REJECTED_ACK: &[u8; 8] = b"CFSHRJ03";
const SHARED_VERSION: u8 = 3;
const SHARED_KIND_FORMAT_SET: u8 = 1;
const BEGIN_BASE_AFTER_MAGIC: usize = 28;
const FORMAT_DESCRIPTOR: usize = 8;
const FINISH_HEADER: usize = 40;
const FINISH_FAILED: u8 = 0;
const FINISH_READY: u8 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SharedFormatDescriptor {
    pub kind: SharedFormatKind,
    pub length: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SharedTextBegin {
    pub event_id: ClipboardEventId,
    pub revision: u64,
    pub formats: Arc<[SharedFormatDescriptor]>,
}

impl SharedTextBegin {
    pub(crate) fn from_snapshot(snapshot: &SharedClipboardSnapshot) -> io::Result<Self> {
        let formats = snapshot
            .formats
            .iter()
            .map(|format| {
                Ok(SharedFormatDescriptor {
                    kind: format.kind,
                    length: 0,
                })
            })
            .collect::<io::Result<Vec<_>>>()?;
        let begin = Self {
            event_id: snapshot.event_id,
            revision: snapshot.revision,
            formats: formats.into(),
        };
        validate_begin(&begin)?;
        Ok(begin)
    }

    pub(crate) fn from_kinds(
        event_id: ClipboardEventId,
        revision: u64,
        kinds: &[SharedFormatKind],
    ) -> io::Result<Self> {
        let begin = Self {
            event_id,
            revision,
            formats: kinds
                .iter()
                .copied()
                .map(|kind| SharedFormatDescriptor { kind, length: 0 })
                .collect::<Vec<_>>()
                .into(),
        };
        validate_begin(&begin)?;
        Ok(begin)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SharedTextFinish {
    Ready(SharedClipboardSnapshot),
    Failed,
}

pub(crate) fn write_begin(stream: &mut impl io::Write, begin: &SharedTextBegin) -> io::Result<()> {
    validate_begin(begin)?;
    let count = u8::try_from(begin.formats.len())
        .map_err(|_| invalid_input("too many shared clipboard formats"))?;
    let mut frame = Vec::with_capacity(
        SHARED_MAGIC.len() + BEGIN_BASE_AFTER_MAGIC + begin.formats.len() * FORMAT_DESCRIPTOR,
    );
    frame.extend_from_slice(SHARED_MAGIC);
    frame.push(SHARED_VERSION);
    frame.push(SHARED_KIND_FORMAT_SET);
    frame.push(count);
    frame.push(0);
    frame.extend_from_slice(&begin.event_id.0);
    frame.extend_from_slice(&begin.revision.to_be_bytes());
    for format in begin.formats.iter().copied() {
        frame.push(format.kind as u8);
        frame.extend_from_slice(&[0; 3]);
        frame.extend_from_slice(&format.length.to_be_bytes());
    }
    stream.write_all(&frame)?;
    stream.flush()
}

pub(crate) fn read_begin_after_magic(stream: &mut impl io::Read) -> io::Result<SharedTextBegin> {
    let mut header = [0_u8; BEGIN_BASE_AFTER_MAGIC];
    stream.read_exact(&mut header)?;
    if header[0] != SHARED_VERSION {
        return Err(invalid_data("unsupported shared clipboard frame version"));
    }
    if header[1] != SHARED_KIND_FORMAT_SET {
        return Err(invalid_data("unknown shared clipboard content kind"));
    }
    if header[3] != 0 {
        return Err(invalid_data(
            "shared clipboard begin reserved field must be zero",
        ));
    }
    let count = usize::from(header[2]);
    if count == 0 || count > MAX_SHARED_FORMATS {
        return Err(invalid_data("shared clipboard format count is invalid"));
    }
    let event_id = ClipboardEventId(
        header[4..20]
            .try_into()
            .map_err(|_| invalid_data("invalid shared clipboard event id"))?,
    );
    let revision = u64::from_be_bytes(
        header[20..28]
            .try_into()
            .map_err(|_| invalid_data("invalid shared clipboard revision"))?,
    );
    let mut formats = Vec::with_capacity(count);
    for _ in 0..count {
        let mut descriptor = [0_u8; FORMAT_DESCRIPTOR];
        stream.read_exact(&mut descriptor)?;
        if descriptor[1..4] != [0; 3] {
            return Err(invalid_data(
                "shared clipboard format reserved fields must be zero",
            ));
        }
        formats.push(SharedFormatDescriptor {
            kind: SharedFormatKind::from_wire(descriptor[0])?,
            length: u32::from_be_bytes(
                descriptor[4..8]
                    .try_into()
                    .map_err(|_| invalid_data("invalid shared clipboard format length"))?,
            ),
        });
    }
    let begin = SharedTextBegin {
        event_id,
        revision,
        formats: formats.into(),
    };
    validate_begin(&begin)?;
    Ok(begin)
}

pub(crate) fn write_ready(
    stream: &mut impl io::Write,
    snapshot: &SharedClipboardSnapshot,
) -> io::Result<()> {
    let begin = SharedTextBegin::from_snapshot(snapshot)?;
    let trace = super::shared_diagnostics::SharedTrace::new(&begin, "send_payload");
    let prepare = trace.stage("body_prepare");
    validate_format_set(&snapshot.formats)?;
    let descriptors = snapshot
        .formats
        .iter()
        .map(|format| {
            Ok(SharedFormatDescriptor {
                kind: format.kind,
                length: u32::try_from(format.bytes.len())
                    .map_err(|_| invalid_input("shared clipboard format is too large"))?,
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    let total = descriptor_total(&descriptors)?;
    let mut hasher = Sha256::new();
    for format in snapshot.formats.iter() {
        hasher.update(&format.bytes);
    }
    let digest: [u8; 32] = hasher.finalize().into();
    let mut header = Vec::with_capacity(FINISH_HEADER);
    header.push(FINISH_READY);
    header.push(
        u8::try_from(begin.formats.len())
            .map_err(|_| invalid_input("too many shared clipboard formats"))?,
    );
    header.extend_from_slice(&[0; 2]);
    header.extend_from_slice(
        &u32::try_from(total)
            .map_err(|_| invalid_input("shared clipboard snapshot is too large"))?
            .to_be_bytes(),
    );
    header.extend_from_slice(&digest);
    prepare.finish("ok");
    trace.mark("payload_size", &format!("bytes={total}"));
    let write = trace.stage("body_write_flush");
    stream.write_all(&header)?;
    for descriptor in descriptors {
        stream.write_all(&[descriptor.kind as u8, 0, 0, 0])?;
        stream.write_all(&descriptor.length.to_be_bytes())?;
    }
    for format in snapshot.formats.iter() {
        stream.write_all(&format.bytes)?;
    }
    let result = stream.flush();
    write.finish(if result.is_ok() { "ok" } else { "failed" });
    result
}

pub(crate) fn write_failed(stream: &mut impl io::Write) -> io::Result<()> {
    stream.write_all(&[0_u8; FINISH_HEADER])?;
    stream.flush()
}

pub(crate) fn read_finish(
    stream: &mut impl io::Read,
    begin: &SharedTextBegin,
) -> io::Result<SharedTextFinish> {
    let trace = super::shared_diagnostics::SharedTrace::new(begin, "receive_payload");
    let header_read = trace.stage("finish_header_read");
    validate_begin(begin)?;
    let mut header = [0_u8; FINISH_HEADER];
    stream.read_exact(&mut header)?;
    if header[2..4] != [0; 2] {
        return Err(invalid_data(
            "shared clipboard finish reserved fields must be zero",
        ));
    }
    let announced_total = usize::try_from(u32::from_be_bytes(
        header[4..8]
            .try_into()
            .map_err(|_| invalid_data("invalid shared clipboard total length"))?,
    ))
    .map_err(|_| invalid_data("shared clipboard total length does not fit this platform"))?;
    if header[0] == FINISH_FAILED {
        if header[1] != 0 || announced_total != 0 || header[8..] != [0; 32] {
            return Err(invalid_data("invalid failed shared clipboard finish"));
        }
        header_read.finish("sender_failed");
        return Ok(SharedTextFinish::Failed);
    }
    if header[0] != FINISH_READY {
        return Err(invalid_data("unknown shared clipboard finish status"));
    }
    let count = usize::from(header[1]);
    if count != begin.formats.len() {
        return Err(invalid_data(
            "shared clipboard finish format count does not match its begin frame",
        ));
    }
    let mut descriptors = Vec::with_capacity(count);
    for (index, announced) in begin.formats.iter().enumerate() {
        let mut descriptor = [0_u8; FORMAT_DESCRIPTOR];
        stream.read_exact(&mut descriptor)?;
        if descriptor[1..4] != [0; 3] {
            return Err(invalid_data(
                "shared clipboard finish descriptor reserved fields must be zero",
            ));
        }
        let kind = SharedFormatKind::from_wire(descriptor[0])?;
        if kind != announced.kind {
            return Err(invalid_data(format!(
                "shared clipboard finish format {index} does not match its begin frame"
            )));
        }
        descriptors.push(SharedFormatDescriptor {
            kind,
            length: u32::from_be_bytes(
                descriptor[4..8]
                    .try_into()
                    .map_err(|_| invalid_data("invalid shared clipboard format length"))?,
            ),
        });
    }
    validate_payload_descriptors(&descriptors)?;
    let expected_total = descriptor_total(&descriptors)?;
    if announced_total != expected_total {
        return Err(invalid_data(
            "shared clipboard finish length does not match its descriptors",
        ));
    }
    let mut payload = vec![0_u8; announced_total];
    header_read.finish("ok");
    trace.mark("payload_size", &format!("bytes={announced_total}"));
    trace.io("body_read", || stream.read_exact(&mut payload))?;
    let validation = trace.stage("body_validate");
    let digest: [u8; 32] = Sha256::digest(&payload).into();
    if digest != header[8..40] {
        return Err(invalid_data("shared clipboard payload digest mismatch"));
    }
    let mut formats = Vec::with_capacity(begin.formats.len());
    let mut offset = 0_usize;
    for descriptor in descriptors {
        let length = usize::try_from(descriptor.length)
            .map_err(|_| invalid_data("shared clipboard format length is too large"))?;
        let end = offset
            .checked_add(length)
            .ok_or_else(|| invalid_data("shared clipboard payload offset overflow"))?;
        formats.push(SharedFormatData::new(
            descriptor.kind,
            Arc::from(&payload[offset..end]),
        )?);
        offset = end;
    }
    let snapshot =
        SharedClipboardSnapshot::received(begin.event_id, begin.revision, formats.into())?;
    validation.finish("ok");
    Ok(SharedTextFinish::Ready(snapshot))
}

fn validate_begin(begin: &SharedTextBegin) -> io::Result<()> {
    if begin.revision == 0 {
        return Err(invalid_input("shared clipboard revision must be non-zero"));
    }
    if begin.formats.is_empty() || begin.formats.len() > MAX_SHARED_FORMATS {
        return Err(invalid_input("shared clipboard format count is invalid"));
    }
    let mut kinds = HashSet::with_capacity(begin.formats.len());
    for format in begin.formats.iter().copied() {
        if !kinds.insert(format.kind) {
            return Err(invalid_input("shared clipboard format is duplicated"));
        }
        if format.length != 0 {
            return Err(invalid_input(
                "shared clipboard begin descriptors must not claim payload lengths",
            ));
        }
    }
    Ok(())
}

fn validate_payload_descriptors(formats: &[SharedFormatDescriptor]) -> io::Result<()> {
    for format in formats.iter().copied() {
        let length = usize::try_from(format.length)
            .map_err(|_| invalid_data("shared clipboard format is too large"))?;
        let valid = match format.kind {
            SharedFormatKind::UnicodeText => {
                length <= MAX_UNICODE_TEXT_BYTES && length.is_multiple_of(2)
            }
            SharedFormatKind::Html | SharedFormatKind::Rtf => {
                length != 0 && length <= MAX_RICH_TEXT_BYTES
            }
            SharedFormatKind::Png | SharedFormatKind::DibV5 | SharedFormatKind::Dib => length != 0,
        };
        if !valid {
            return Err(invalid_data(
                "shared clipboard format length is outside its limit",
            ));
        }
    }
    Ok(())
}

fn descriptor_total(formats: &[SharedFormatDescriptor]) -> io::Result<usize> {
    let total = formats.iter().try_fold(0_usize, |total, format| {
        total
            .checked_add(
                usize::try_from(format.length)
                    .map_err(|_| invalid_data("shared clipboard format is too large"))?,
            )
            .ok_or_else(|| invalid_data("shared clipboard total length overflow"))
    })?;
    if total > MAX_SHARED_SNAPSHOT_BYTES {
        return Err(invalid_data(
            "shared clipboard snapshot exceeds its fixed limit",
        ));
    }
    Ok(total)
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_format(units: &[u16]) -> SharedFormatData {
        let mut bytes = Vec::with_capacity(units.len() * 2);
        for unit in units {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        SharedFormatData::new(SharedFormatKind::UnicodeText, bytes.into()).unwrap()
    }

    fn sample() -> SharedClipboardSnapshot {
        SharedClipboardSnapshot::received(
            ClipboardEventId([0x5A; 16]),
            42,
            Arc::from([
                text_format(&[0x4F60, 0x597D, 0xD83D, 0xDE80]),
                SharedFormatData::new(
                    SharedFormatKind::Rtf,
                    Arc::from(b"{\\rtf1 hello}\0".as_slice()),
                )
                .unwrap(),
            ]),
        )
        .unwrap()
    }

    #[test]
    fn two_phase_multi_format_snapshot_round_trips() {
        let snapshot = sample();
        let begin = SharedTextBegin::from_snapshot(&snapshot).unwrap();
        let mut bytes = Vec::new();
        write_begin(&mut bytes, &begin).unwrap();
        write_ready(&mut bytes, &snapshot).unwrap();
        assert_eq!(&bytes[..8], SHARED_MAGIC);
        let mut input = &bytes[8..];
        let decoded_begin = read_begin_after_magic(&mut input).unwrap();
        assert_eq!(decoded_begin, begin);
        let SharedTextFinish::Ready(decoded) = read_finish(&mut input, &decoded_begin).unwrap()
        else {
            panic!("expected ready snapshot");
        };
        assert_eq!(decoded, snapshot);
    }

    #[test]
    fn failed_finish_is_explicit_and_strict() {
        let begin = SharedTextBegin::from_snapshot(&sample()).unwrap();
        let mut bytes = Vec::new();
        write_failed(&mut bytes).unwrap();
        assert_eq!(
            read_finish(&mut &bytes[..], &begin).unwrap(),
            SharedTextFinish::Failed
        );
        bytes[8] = 1;
        assert!(read_finish(&mut &bytes[..], &begin).is_err());
    }

    #[test]
    fn ready_finish_rejects_truncation_digest_and_descriptor_confusion() {
        let snapshot = sample();
        let begin = SharedTextBegin::from_snapshot(&snapshot).unwrap();
        let mut encoded = Vec::new();
        write_ready(&mut encoded, &snapshot).unwrap();
        for end in 0..encoded.len() {
            assert!(read_finish(&mut &encoded[..end], &begin).is_err());
        }
        *encoded.last_mut().unwrap() ^= 1;
        assert!(read_finish(&mut &encoded[..], &begin).is_err());

        let mut oversized = begin;
        Arc::make_mut(&mut oversized.formats)[0].length =
            u32::try_from(MAX_UNICODE_TEXT_BYTES + 2).unwrap();
        assert!(validate_begin(&oversized).is_err());
    }
}
