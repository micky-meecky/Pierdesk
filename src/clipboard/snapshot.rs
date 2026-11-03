use std::collections::HashSet;
use std::io::{self, Read as _, Write as _};
use std::mem::size_of;
use std::os::windows::process::CommandExt as _;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HGLOBAL;
use windows::Win32::System::DataExchange::{
    GetClipboardData, GetClipboardSequenceNumber, IsClipboardFormatAvailable,
    RegisterClipboardFormatW,
};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::{CF_DIB, CF_DIBV5, CF_UNICODETEXT};
use windows::core::{Error, PCWSTR, Result as WindowsResult, w};

use super::local_file::ClipboardGuard;

pub(crate) const MAX_UNICODE_TEXT_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MAX_RICH_TEXT_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_SHARED_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const MAX_IMAGE_DECODED_BYTES: usize = 128 * 1024 * 1024;
pub(crate) const MAX_IMAGE_DIMENSION: u32 = 16_384;
pub(crate) const MAX_SHARED_FORMATS: usize = 8;
pub(crate) const HTML_FORMAT_NAME: PCWSTR = w!("HTML Format");
pub(crate) const RTF_FORMAT_NAME: PCWSTR = w!("Rich Text Format");
pub(crate) const PNG_FORMAT_NAME: PCWSTR = w!("PNG");
pub(crate) const SHARED_ORIGIN_FORMAT: PCWSTR = w!("ClipFerry.SharedClipboardOrigin.v1");
pub(crate) const SHARED_ORIGIN_PREFIX: &[u8] = b"ClipFerry.SharedClipboardOrigin.v1\0";
const EXCLUDE_MONITOR_FORMAT: PCWSTR = w!("ExcludeClipboardContentFromMonitorProcessing");
const CAN_UPLOAD_TO_CLOUD_FORMAT: PCWSTR = w!("CanUploadToCloudClipboard");
const CAPTURE_HELPER_POLICY_ENV: &str = "PIERDESK_CAPTURE_POLICY";
const CAPTURE_HELPER_MAGIC: &[u8; 8] = b"CFSCAP02";
const CAPTURE_HELPER_HEADER: usize = 8 + 1 + 1 + 2 + 4;
const CAPTURE_HELPER_DESCRIPTOR: usize = 8;
const CAPTURE_HELPER_TIMEOUT: Duration = Duration::from_secs(5);
const CAPTURE_HELPER_STDERR_LIMIT: usize = 8 * 1024;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ClipboardEventId(pub [u8; 16]);

impl ClipboardEventId {
    pub(crate) fn random() -> io::Result<Self> {
        let mut bytes = [0_u8; 16];
        getrandom::fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
        Ok(Self(bytes))
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub(crate) enum SharedFormatKind {
    UnicodeText = 1,
    Html = 2,
    Rtf = 3,
    Png = 4,
    DibV5 = 5,
    Dib = 6,
}

impl SharedFormatKind {
    pub(crate) fn from_wire(value: u8) -> io::Result<Self> {
        match value {
            1 => Ok(Self::UnicodeText),
            2 => Ok(Self::Html),
            3 => Ok(Self::Rtf),
            4 => Ok(Self::Png),
            5 => Ok(Self::DibV5),
            6 => Ok(Self::Dib),
            _ => Err(invalid_data("unknown shared clipboard format")),
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::UnicodeText => "文字",
            Self::Html => "HTML",
            Self::Rtf => "RTF",
            Self::Png => "PNG",
            Self::DibV5 => "DIBV5",
            Self::Dib => "DIB",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SharedFormatData {
    pub kind: SharedFormatKind,
    /// Unicode text is UTF-16LE without its terminating NUL. Other variants are complete
    /// HGLOBAL payloads ready to publish on the receiving computer.
    pub bytes: Arc<[u8]>,
}

impl SharedFormatData {
    pub(crate) fn new(kind: SharedFormatKind, bytes: Arc<[u8]>) -> io::Result<Self> {
        validate_format_payload(kind, &bytes)?;
        Ok(Self { kind, bytes })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SharedClipboardSnapshot {
    pub event_id: ClipboardEventId,
    pub revision: u64,
    pub sequence: u32,
    pub formats: Arc<[SharedFormatData]>,
}

impl SharedClipboardSnapshot {
    pub(crate) fn received(
        event_id: ClipboardEventId,
        revision: u64,
        formats: Arc<[SharedFormatData]>,
    ) -> io::Result<Self> {
        if revision == 0 {
            return Err(invalid_data("shared clipboard revision must be non-zero"));
        }
        validate_format_set(&formats)?;
        Ok(Self {
            event_id,
            revision,
            sequence: 0,
            formats,
        })
    }

    pub(crate) fn total_bytes(&self) -> usize {
        self.formats.iter().map(|format| format.bytes.len()).sum()
    }

    pub(crate) fn unicode_units(&self) -> usize {
        self.formats
            .iter()
            .find(|format| format.kind == SharedFormatKind::UnicodeText)
            .map_or(0, |format| format.bytes.len() / 2)
    }

    pub(crate) fn format_labels(&self) -> String {
        self.formats
            .iter()
            .map(|format| format.kind.label())
            .collect::<Vec<_>>()
            .join("+")
    }

    #[cfg(test)]
    pub(crate) fn origin_payload(&self) -> Vec<u8> {
        let mut payload = Vec::with_capacity(SHARED_ORIGIN_PREFIX.len() + 16);
        payload.extend_from_slice(SHARED_ORIGIN_PREFIX);
        payload.extend_from_slice(&self.event_id.0);
        payload
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SharedCapturePolicy {
    pub text: bool,
    pub rich_text: bool,
    pub image: bool,
}

impl SharedCapturePolicy {
    pub(crate) const fn any(self) -> bool {
        self.text || self.rich_text || self.image
    }

    fn encode(self) -> String {
        format!(
            "{}{}{}",
            u8::from(self.text),
            u8::from(self.rich_text),
            u8::from(self.image)
        )
    }

    fn from_environment() -> io::Result<Self> {
        let value = std::env::var(CAPTURE_HELPER_POLICY_ENV).unwrap_or_default();
        if value.len() != 3 || value.bytes().any(|byte| !matches!(byte, b'0' | b'1')) {
            return Err(invalid_data(
                "shared capture helper policy is missing or invalid",
            ));
        }
        let bytes = value.as_bytes();
        Ok(Self {
            text: bytes[0] == b'1',
            rich_text: bytes[1] == b'1',
            image: bytes[2] == b'1',
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SharedCaptureFormats {
    origin: u32,
    exclude_monitor: u32,
    can_upload_to_cloud: u32,
    html: u32,
    rtf: u32,
    png: u32,
}

impl SharedCaptureFormats {
    pub(crate) fn register() -> WindowsResult<Self> {
        Ok(Self {
            origin: register_format(SHARED_ORIGIN_FORMAT)?,
            exclude_monitor: register_format(EXCLUDE_MONITOR_FORMAT)?,
            can_upload_to_cloud: register_format(CAN_UPLOAD_TO_CLOUD_FORMAT)?,
            html: register_format(HTML_FORMAT_NAME)?,
            rtf: register_format(RTF_FORMAT_NAME)?,
            png: register_format(PNG_FORMAT_NAME)?,
        })
    }

    pub(crate) fn private_shared_available(self) -> bool {
        unsafe { IsClipboardFormatAvailable(self.origin) }.is_ok()
    }

    pub(crate) fn announced_kinds(self, policy: SharedCapturePolicy) -> Arc<[SharedFormatKind]> {
        if self.private_shared_available()
            || unsafe { IsClipboardFormatAvailable(self.exclude_monitor) }.is_ok()
            || self.cloud_upload_is_prohibited()
        {
            return Arc::from([]);
        }
        let mut kinds = Vec::with_capacity(6);
        let html_available =
            policy.rich_text && unsafe { IsClipboardFormatAvailable(self.html) }.is_ok();
        let rtf_available =
            policy.rich_text && unsafe { IsClipboardFormatAvailable(self.rtf) }.is_ok();
        if (policy.text || html_available || rtf_available)
            && unsafe { IsClipboardFormatAvailable(u32::from(CF_UNICODETEXT.0)) }.is_ok()
        {
            kinds.push(SharedFormatKind::UnicodeText);
        }
        if html_available {
            kinds.push(SharedFormatKind::Html);
        }
        if rtf_available {
            kinds.push(SharedFormatKind::Rtf);
        }
        if policy.image {
            if unsafe { IsClipboardFormatAvailable(self.png) }.is_ok() {
                kinds.push(SharedFormatKind::Png);
            } else if unsafe { IsClipboardFormatAvailable(u32::from(CF_DIBV5.0)) }.is_ok() {
                kinds.push(SharedFormatKind::DibV5);
            } else if unsafe { IsClipboardFormatAvailable(u32::from(CF_DIB.0)) }.is_ok() {
                kinds.push(SharedFormatKind::Dib);
            }
        }
        kinds.into()
    }

    fn cloud_upload_is_prohibited(self) -> bool {
        if unsafe { IsClipboardFormatAvailable(self.can_upload_to_cloud) }.is_err() {
            return false;
        }
        let Ok(_clipboard) = ClipboardGuard::open(None) else {
            // Failure to verify an explicit upload policy is handled conservatively.
            return true;
        };
        read_clipboard_u32(self.can_upload_to_cloud).ok() != Some(1)
    }
}

#[derive(Debug)]
pub(crate) enum SharedClipboardCapture {
    Snapshot {
        formats: Arc<[SharedFormatData]>,
        sequence: u32,
    },
    NotShareable,
    PrivateShared,
    Excluded,
    ChangedDuringRead,
}

pub(crate) fn capture_shared_clipboard(
    registered: SharedCaptureFormats,
    policy: SharedCapturePolicy,
) -> io::Result<SharedClipboardCapture> {
    let _clipboard = ClipboardGuard::open(None)
        .map_err(|error| capture_windows_error("open_clipboard", None, &error))?;
    let sequence_before = unsafe { GetClipboardSequenceNumber() };
    if unsafe { IsClipboardFormatAvailable(registered.origin) }.is_ok()
        && read_shared_origin(registered.origin)
    {
        return Ok(SharedClipboardCapture::PrivateShared);
    }
    if unsafe { IsClipboardFormatAvailable(registered.exclude_monitor) }.is_ok() {
        return Ok(SharedClipboardCapture::Excluded);
    }
    if unsafe { IsClipboardFormatAvailable(registered.can_upload_to_cloud) }.is_ok()
        && read_clipboard_u32(registered.can_upload_to_cloud)
            .map_err(|error| capture_windows_error("read_cloud_upload_policy", None, &error))?
            != 1
    {
        return Ok(SharedClipboardCapture::Excluded);
    }

    let mut formats = Vec::with_capacity(6);
    let unicode_available =
        unsafe { IsClipboardFormatAvailable(u32::from(CF_UNICODETEXT.0)) }.is_ok();
    let html_available =
        policy.rich_text && unsafe { IsClipboardFormatAvailable(registered.html) }.is_ok();
    let rtf_available =
        policy.rich_text && unsafe { IsClipboardFormatAvailable(registered.rtf) }.is_ok();
    if unicode_available && (policy.text || html_available || rtf_available) {
        formats.push(SharedFormatData {
            kind: SharedFormatKind::UnicodeText,
            bytes: read_unicode_text().map_err(|error| {
                capture_windows_error("read_format", Some(SharedFormatKind::UnicodeText), &error)
            })?,
        });
    }
    capture_rich_text_formats(registered, html_available, rtf_available, &mut formats)?;
    capture_image_format(registered, policy.image, &mut formats)?;
    if formats.is_empty() {
        return Ok(SharedClipboardCapture::NotShareable);
    }
    validate_format_set(&formats)
        .map_err(|error| capture_data_error("validate_snapshot", None, &error))?;
    let sequence_after = unsafe { GetClipboardSequenceNumber() };
    if sequence_before != 0 && sequence_after != sequence_before {
        return Ok(SharedClipboardCapture::ChangedDuringRead);
    }
    Ok(SharedClipboardCapture::Snapshot {
        formats: formats.into(),
        sequence: sequence_after,
    })
}

fn capture_rich_text_formats(
    registered: SharedCaptureFormats,
    html_available: bool,
    rtf_available: bool,
    formats: &mut Vec<SharedFormatData>,
) -> io::Result<()> {
    if html_available {
        let source = read_hglobal_bytes(registered.html, MAX_RICH_TEXT_BYTES).map_err(|error| {
            capture_windows_error("read_format", Some(SharedFormatKind::Html), &error)
        })?;
        let bytes = normalize_html(source).map_err(|error| {
            capture_data_error("normalize_format", Some(SharedFormatKind::Html), &error)
        })?;
        formats.push(SharedFormatData {
            kind: SharedFormatKind::Html,
            bytes: bytes.into(),
        });
    }
    if rtf_available {
        let source = read_hglobal_bytes(registered.rtf, MAX_RICH_TEXT_BYTES).map_err(|error| {
            capture_windows_error("read_format", Some(SharedFormatKind::Rtf), &error)
        })?;
        let bytes = normalize_rtf(source).map_err(|error| {
            capture_data_error("normalize_format", Some(SharedFormatKind::Rtf), &error)
        })?;
        formats.push(SharedFormatData {
            kind: SharedFormatKind::Rtf,
            bytes: bytes.into(),
        });
    }
    Ok(())
}

fn capture_image_format(
    registered: SharedCaptureFormats,
    enabled: bool,
    formats: &mut Vec<SharedFormatData>,
) -> io::Result<()> {
    let (kind, bytes) = if !enabled {
        return Ok(());
    } else if unsafe { IsClipboardFormatAvailable(registered.png) }.is_ok() {
        let source =
            read_hglobal_bytes(registered.png, MAX_SHARED_SNAPSHOT_BYTES).map_err(|error| {
                capture_windows_error("read_format", Some(SharedFormatKind::Png), &error)
            })?;
        let bytes = normalize_png(&source).map_err(|error| {
            capture_data_error("normalize_format", Some(SharedFormatKind::Png), &error)
        })?;
        (SharedFormatKind::Png, bytes)
    } else if unsafe { IsClipboardFormatAvailable(u32::from(CF_DIBV5.0)) }.is_ok() {
        let bytes = read_hglobal_bytes(u32::from(CF_DIBV5.0), MAX_SHARED_SNAPSHOT_BYTES).map_err(
            |error| capture_windows_error("read_format", Some(SharedFormatKind::DibV5), &error),
        )?;
        validate_dib(&bytes, true).map_err(|error| {
            capture_data_error("validate_format", Some(SharedFormatKind::DibV5), &error)
        })?;
        (SharedFormatKind::DibV5, bytes)
    } else if unsafe { IsClipboardFormatAvailable(u32::from(CF_DIB.0)) }.is_ok() {
        let bytes = read_hglobal_bytes(u32::from(CF_DIB.0), MAX_SHARED_SNAPSHOT_BYTES).map_err(
            |error| capture_windows_error("read_format", Some(SharedFormatKind::Dib), &error),
        )?;
        validate_dib(&bytes, false).map_err(|error| {
            capture_data_error("validate_format", Some(SharedFormatKind::Dib), &error)
        })?;
        (SharedFormatKind::Dib, bytes)
    } else {
        return Ok(());
    };
    formats.push(SharedFormatData {
        kind,
        bytes: bytes.into(),
    });
    Ok(())
}

/// Runs the short-lived child side of isolated multi-format clipboard capture.
///
/// # Errors
///
/// Returns an error when clipboard registration/read or the bounded stdout frame fails.
pub fn run_shared_clipboard_capture_helper() -> io::Result<()> {
    let policy = SharedCapturePolicy::from_environment()?;
    let formats = SharedCaptureFormats::register()
        .map_err(|error| capture_windows_error("register_formats", None, &error))?;
    let capture = capture_shared_clipboard(formats, policy)?;
    let frame = encode_capture_helper_result(&capture)?;
    let stdout = io::stdout();
    let mut output = stdout.lock();
    output.write_all(&frame)?;
    output.flush()
}

pub(crate) fn capture_shared_clipboard_isolated(
    cancelled: &AtomicBool,
    latest_generation: &std::sync::atomic::AtomicU64,
    expected_generation: u64,
    policy: SharedCapturePolicy,
) -> Result<SharedClipboardCapture, String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let mut child = Command::new(executable)
        .arg("shared-text-capture-helper")
        .env(CAPTURE_HELPER_POLICY_ENV, policy.encode())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|error| format!("could not start clipboard capture helper: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "clipboard capture helper stdout was not piped".to_owned())?;
    let reader = std::thread::Builder::new()
        .name("pierdesk-shared-helper-read".to_owned())
        .spawn(move || {
            let limit = u64::try_from(
                CAPTURE_HELPER_HEADER
                    + MAX_SHARED_FORMATS * CAPTURE_HELPER_DESCRIPTOR
                    + MAX_SHARED_SNAPSHOT_BYTES
                    + 1,
            )
            .expect("capture helper limit fits u64");
            let mut bytes = Vec::new();
            stdout.take(limit).read_to_end(&mut bytes).map(|_| bytes)
        })
        .map_err(|error| format!("could not read clipboard capture helper: {error}"))?;
    let Some(stderr) = child.stderr.take() else {
        let _ = child.kill();
        let _ = child.wait();
        let _ = reader.join();
        return Err("clipboard capture helper stderr was not piped".to_owned());
    };
    let diagnostic_reader = match std::thread::Builder::new()
        .name("pierdesk-shared-helper-stderr".to_owned())
        .spawn(move || read_bounded_helper_diagnostic(stderr, CAPTURE_HELPER_STDERR_LIMIT))
    {
        Ok(reader) => reader,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Err(format!(
                "could not read clipboard capture helper diagnostics: {error}"
            ));
        }
    };
    let deadline = Instant::now()
        .checked_add(CAPTURE_HELPER_TIMEOUT)
        .unwrap_or_else(Instant::now);
    let status = match wait_for_capture_helper(
        &mut child,
        cancelled,
        latest_generation,
        expected_generation,
        deadline,
    ) {
        Ok(status) => status,
        Err(error) => {
            let _ = reader.join();
            let diagnostic = join_helper_diagnostic(diagnostic_reader);
            return Err(with_helper_diagnostic(error, &diagnostic));
        }
    };
    let bytes = reader.join();
    let diagnostic = join_helper_diagnostic(diagnostic_reader);
    let bytes = bytes
        .map_err(|_| {
            with_helper_diagnostic(
                "clipboard capture helper reader panicked".to_owned(),
                &diagnostic,
            )
        })?
        .map_err(|error| {
            with_helper_diagnostic(
                format!("clipboard capture helper output failed: {error}"),
                &diagnostic,
            )
        })?;
    if !status.success() {
        return Err(with_helper_diagnostic(
            format!("clipboard capture helper exited with status {status}"),
            &diagnostic,
        ));
    }
    decode_capture_helper_result(&bytes)
        .map_err(|error| with_helper_diagnostic(error.to_string(), &diagnostic))
}

fn wait_for_capture_helper(
    child: &mut std::process::Child,
    cancelled: &AtomicBool,
    latest_generation: &std::sync::atomic::AtomicU64,
    expected_generation: u64,
    deadline: Instant,
) -> Result<std::process::ExitStatus, String> {
    loop {
        let superseded = latest_generation.load(Ordering::Acquire) != expected_generation;
        if cancelled.load(Ordering::Acquire) || superseded || Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(if cancelled.load(Ordering::Acquire) || superseded {
                "clipboard capture was cancelled".to_owned()
            } else {
                "clipboard source did not render supported content within 5 seconds".to_owned()
            });
        }
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("clipboard capture helper failed: {error}"));
            }
        }
    }
}

#[derive(Debug)]
struct BoundedHelperDiagnostic {
    bytes: Vec<u8>,
    truncated: bool,
}

fn read_bounded_helper_diagnostic(
    mut input: impl io::Read,
    limit: usize,
) -> io::Result<BoundedHelperDiagnostic> {
    let read_limit = limit.saturating_add(1);
    let mut bytes = Vec::with_capacity(read_limit.min(1024));
    input
        .by_ref()
        .take(u64::try_from(read_limit).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)?;
    let truncated = bytes.len() > limit;
    bytes.truncate(limit);
    Ok(BoundedHelperDiagnostic { bytes, truncated })
}

fn join_helper_diagnostic(
    reader: std::thread::JoinHandle<io::Result<BoundedHelperDiagnostic>>,
) -> BoundedHelperDiagnostic {
    match reader.join() {
        Ok(Ok(diagnostic)) => diagnostic,
        Ok(Err(error)) => BoundedHelperDiagnostic {
            bytes: format!("stderr_read_error={error}").into_bytes(),
            truncated: false,
        },
        Err(_) => BoundedHelperDiagnostic {
            bytes: b"stderr_reader_panicked".to_vec(),
            truncated: false,
        },
    }
}

fn with_helper_diagnostic(mut error: String, diagnostic: &BoundedHelperDiagnostic) -> String {
    let summary = String::from_utf8_lossy(&diagnostic.bytes)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if !summary.is_empty() {
        error.push_str("; helper_stderr=");
        error.push_str(&summary);
        if diagnostic.truncated {
            error.push_str(" [truncated]");
        }
    }
    error
}

fn encode_capture_helper_result(capture: &SharedClipboardCapture) -> io::Result<Vec<u8>> {
    let (status, sequence, formats): (u8, u32, &[SharedFormatData]) = match capture {
        SharedClipboardCapture::NotShareable => (0, 0, &[]),
        SharedClipboardCapture::PrivateShared => (1, 0, &[]),
        SharedClipboardCapture::ChangedDuringRead => (2, 0, &[]),
        SharedClipboardCapture::Excluded => (3, 0, &[]),
        SharedClipboardCapture::Snapshot { formats, sequence } => (4, *sequence, formats),
    };
    let count = u8::try_from(formats.len()).map_err(|_| invalid_data("too many formats"))?;
    let payload_len = formats
        .iter()
        .map(|format| format.bytes.len())
        .sum::<usize>();
    let mut frame = Vec::with_capacity(
        CAPTURE_HELPER_HEADER + formats.len() * CAPTURE_HELPER_DESCRIPTOR + payload_len,
    );
    frame.extend_from_slice(CAPTURE_HELPER_MAGIC);
    frame.push(status);
    frame.push(count);
    frame.extend_from_slice(&[0; 2]);
    frame.extend_from_slice(&sequence.to_be_bytes());
    for format in formats {
        frame.push(format.kind as u8);
        frame.extend_from_slice(&[0; 3]);
        let length = u32::try_from(format.bytes.len())
            .map_err(|_| invalid_data("captured format is too large"))?;
        frame.extend_from_slice(&length.to_be_bytes());
    }
    for format in formats {
        frame.extend_from_slice(&format.bytes);
    }
    Ok(frame)
}

fn decode_capture_helper_result(frame: &[u8]) -> io::Result<SharedClipboardCapture> {
    if frame.len() < CAPTURE_HELPER_HEADER || &frame[..8] != CAPTURE_HELPER_MAGIC {
        return Err(invalid_data("invalid clipboard capture helper frame"));
    }
    if frame[10..12] != [0; 2] {
        return Err(invalid_data(
            "clipboard capture helper reserved fields are not zero",
        ));
    }
    let status = frame[8];
    let count = usize::from(frame[9]);
    let sequence = u32::from_be_bytes(
        frame[12..16]
            .try_into()
            .map_err(|_| invalid_data("missing clipboard sequence"))?,
    );
    let descriptor_bytes = count
        .checked_mul(CAPTURE_HELPER_DESCRIPTOR)
        .ok_or_else(|| invalid_data("capture descriptor length overflow"))?;
    let payload_start = CAPTURE_HELPER_HEADER
        .checked_add(descriptor_bytes)
        .ok_or_else(|| invalid_data("capture frame length overflow"))?;
    if count > MAX_SHARED_FORMATS || payload_start > frame.len() {
        return Err(invalid_data("capture format count is invalid"));
    }
    if status != 4 {
        return match (status, count, sequence, frame.len()) {
            (0, 0, 0, CAPTURE_HELPER_HEADER) => Ok(SharedClipboardCapture::NotShareable),
            (1, 0, 0, CAPTURE_HELPER_HEADER) => Ok(SharedClipboardCapture::PrivateShared),
            (2, 0, 0, CAPTURE_HELPER_HEADER) => Ok(SharedClipboardCapture::ChangedDuringRead),
            (3, 0, 0, CAPTURE_HELPER_HEADER) => Ok(SharedClipboardCapture::Excluded),
            _ => Err(invalid_data("clipboard capture helper status is invalid")),
        };
    }
    if count == 0 {
        return Err(invalid_data("successful capture has no formats"));
    }
    let mut descriptors = Vec::with_capacity(count);
    let mut total = 0_usize;
    for index in 0..count {
        let offset = CAPTURE_HELPER_HEADER + index * CAPTURE_HELPER_DESCRIPTOR;
        if frame[offset + 1..offset + 4] != [0; 3] {
            return Err(invalid_data("capture format reserved fields are not zero"));
        }
        let kind = SharedFormatKind::from_wire(frame[offset])?;
        let length = usize::try_from(u32::from_be_bytes(
            frame[offset + 4..offset + 8]
                .try_into()
                .map_err(|_| invalid_data("missing capture format length"))?,
        ))
        .map_err(|_| invalid_data("capture format is too large"))?;
        total = total
            .checked_add(length)
            .ok_or_else(|| invalid_data("capture payload length overflow"))?;
        descriptors.push((kind, length));
    }
    if total > MAX_SHARED_SNAPSHOT_BYTES || frame.len() != payload_start + total {
        return Err(invalid_data("capture helper payload length is invalid"));
    }
    let mut formats = Vec::with_capacity(count);
    let mut offset = payload_start;
    for (kind, length) in descriptors {
        let end = offset + length;
        formats.push(SharedFormatData::new(kind, Arc::from(&frame[offset..end]))?);
        offset = end;
    }
    validate_format_set(&formats)?;
    Ok(SharedClipboardCapture::Snapshot {
        formats: formats.into(),
        sequence,
    })
}

fn register_format(name: PCWSTR) -> WindowsResult<u32> {
    let format = unsafe { RegisterClipboardFormatW(name) };
    if format == 0 {
        Err(Error::from_thread())
    } else {
        Ok(format)
    }
}

fn read_unicode_text() -> WindowsResult<Arc<[u8]>> {
    let handle = unsafe { GetClipboardData(u32::from(CF_UNICODETEXT.0)) }?;
    let global = HGLOBAL(handle.0);
    let byte_length = unsafe { GlobalSize(global) };
    if byte_length < size_of::<u16>() {
        return Err(Error::from_hresult(
            windows::Win32::Foundation::E_INVALIDARG,
        ));
    }
    let pointer = unsafe { GlobalLock(global) };
    if pointer.is_null() {
        return Err(Error::from_thread());
    }
    let inspect_units = (byte_length / 2).min(MAX_UNICODE_TEXT_BYTES / 2 + 1);
    let units = unsafe { std::slice::from_raw_parts(pointer.cast::<u16>(), inspect_units) };
    let terminator = units.iter().position(|unit| *unit == 0);
    let mut bytes = Vec::new();
    if let Some(end) = terminator {
        bytes.reserve(end * 2);
        for unit in &units[..end] {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
    }
    let _ = unsafe { GlobalUnlock(global) };
    if terminator.is_none() {
        return Err(Error::from_hresult(
            windows::Win32::Foundation::E_INVALIDARG,
        ));
    }
    validate_unicode_bytes(&bytes)
        .map_err(|_| Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG))?;
    Ok(bytes.into())
}

fn read_hglobal_bytes(format: u32, limit: usize) -> WindowsResult<Vec<u8>> {
    let handle = unsafe { GetClipboardData(format) }?;
    let global = HGLOBAL(handle.0);
    let length = unsafe { GlobalSize(global) };
    if length == 0 || length > limit {
        return Err(Error::from_hresult(
            windows::Win32::Foundation::E_INVALIDARG,
        ));
    }
    let pointer = unsafe { GlobalLock(global) };
    if pointer.is_null() {
        return Err(Error::from_thread());
    }
    let bytes = unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), length) }.to_vec();
    let _ = unsafe { GlobalUnlock(global) };
    Ok(bytes)
}

fn read_clipboard_u32(format: u32) -> WindowsResult<u32> {
    let handle = unsafe { GetClipboardData(format) }?;
    let global = HGLOBAL(handle.0);
    if unsafe { GlobalSize(global) } < size_of::<u32>() {
        return Err(Error::from_hresult(
            windows::Win32::Foundation::E_INVALIDARG,
        ));
    }
    let pointer = unsafe { GlobalLock(global) };
    if pointer.is_null() {
        return Err(Error::from_thread());
    }
    let mut bytes = [0_u8; size_of::<u32>()];
    bytes.copy_from_slice(unsafe {
        std::slice::from_raw_parts(pointer.cast::<u8>(), size_of::<u32>())
    });
    let _ = unsafe { GlobalUnlock(global) };
    Ok(u32::from_le_bytes(bytes))
}

fn normalize_html(mut bytes: Vec<u8>) -> io::Result<Vec<u8>> {
    let header_end = bytes.len().min(4096);
    let header = std::str::from_utf8(&bytes[..header_end])
        .map_err(|_| invalid_data("HTML clipboard header is not UTF-8"))?;
    let start = parse_html_offset(header, "StartHTML:")?;
    let end = parse_html_offset(header, "EndHTML:")?;
    if start >= end || end > bytes.len() {
        return Err(invalid_data("HTML clipboard offsets are invalid"));
    }
    bytes.truncate(end);
    bytes.push(0);
    Ok(bytes)
}

fn parse_html_offset(header: &str, field: &str) -> io::Result<usize> {
    let line = header
        .lines()
        .find_map(|line| line.strip_prefix(field))
        .ok_or_else(|| invalid_data("HTML clipboard offset is missing"))?;
    line.trim()
        .parse::<usize>()
        .map_err(|_| invalid_data("HTML clipboard offset is invalid"))
}

fn normalize_rtf(mut bytes: Vec<u8>) -> io::Result<Vec<u8>> {
    if let Some(nul) = bytes.iter().position(|byte| *byte == 0) {
        bytes.truncate(nul);
    }
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes.pop();
    }
    if !bytes.starts_with(b"{\\rtf") || bytes.last() != Some(&b'}') {
        return Err(invalid_data("RTF clipboard payload is invalid"));
    }
    bytes.push(0);
    Ok(bytes)
}

pub(crate) fn normalize_png(bytes: &[u8]) -> io::Result<Vec<u8>> {
    const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if !bytes.starts_with(SIGNATURE) || bytes.len() < 33 {
        return Err(invalid_data(
            "PNG clipboard payload has an invalid signature",
        ));
    }
    if u32::from_be_bytes(bytes[8..12].try_into().expect("PNG length is present")) != 13
        || &bytes[12..16] != b"IHDR"
    {
        return Err(invalid_data("PNG clipboard payload is missing IHDR"));
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().expect("PNG width is present"));
    let height = u32::from_be_bytes(bytes[20..24].try_into().expect("PNG height is present"));
    validate_image_dimensions(width, height)?;
    let bit_depth = bytes[24];
    let color_type = bytes[25];
    let valid_depth = match color_type {
        0 => matches!(bit_depth, 1 | 2 | 4 | 8 | 16),
        2 | 4 | 6 => matches!(bit_depth, 8 | 16),
        3 => matches!(bit_depth, 1 | 2 | 4 | 8),
        _ => false,
    };
    if !valid_depth || bytes[26] != 0 || bytes[27] != 0 || !matches!(bytes[28], 0 | 1) {
        return Err(invalid_data("PNG IHDR fields are unsupported"));
    }
    let mut offset = 8_usize;
    let mut saw_idat = false;
    loop {
        if offset
            .checked_add(12)
            .is_none_or(|minimum| minimum > bytes.len())
        {
            return Err(invalid_data("PNG chunk header is truncated"));
        }
        let length = usize::try_from(u32::from_be_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("PNG chunk length is present"),
        ))
        .map_err(|_| invalid_data("PNG chunk is too large"))?;
        let end = offset
            .checked_add(12)
            .and_then(|value| value.checked_add(length))
            .ok_or_else(|| invalid_data("PNG chunk length overflow"))?;
        if end > bytes.len() {
            return Err(invalid_data("PNG chunk is truncated"));
        }
        let chunk = &bytes[offset + 4..offset + 8];
        let stored_crc =
            u32::from_be_bytes(bytes[end - 4..end].try_into().expect("PNG CRC is present"));
        if png_crc32(&bytes[offset + 4..end - 4]) != stored_crc {
            return Err(invalid_data("PNG chunk CRC is invalid"));
        }
        if offset == 8 && chunk != b"IHDR" {
            return Err(invalid_data("PNG first chunk is not IHDR"));
        }
        if offset != 8 && chunk == b"IHDR" {
            return Err(invalid_data("PNG contains a duplicate IHDR"));
        }
        if chunk == b"IDAT" {
            saw_idat = true;
        }
        if chunk == b"IEND" {
            if length != 0 || !saw_idat {
                return Err(invalid_data("PNG IEND chunk is invalid"));
            }
            return Ok(bytes[..end].to_vec());
        }
        offset = end;
    }
}

fn png_crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

#[allow(clippy::too_many_lines)]
fn validate_dib(bytes: &[u8], require_v5: bool) -> io::Result<()> {
    if bytes.len() < 40 {
        return Err(invalid_data("DIB header is truncated"));
    }
    let header_size = usize::try_from(u32::from_le_bytes(
        bytes[0..4].try_into().expect("DIB header size is present"),
    ))
    .map_err(|_| invalid_data("DIB header is too large"))?;
    if header_size < 40 || header_size > bytes.len() || (require_v5 && header_size < 124) {
        return Err(invalid_data("DIB header size is unsupported"));
    }
    let width = i32::from_le_bytes(bytes[4..8].try_into().expect("DIB width is present"));
    let height = i32::from_le_bytes(bytes[8..12].try_into().expect("DIB height is present"));
    if width <= 0 || height == 0 {
        return Err(invalid_data("DIB dimensions are invalid"));
    }
    let width = u32::try_from(width).map_err(|_| invalid_data("DIB width is invalid"))?;
    let height = height.unsigned_abs();
    validate_image_dimensions(width, height)?;
    let planes = u16::from_le_bytes(bytes[12..14].try_into().expect("DIB planes are present"));
    let bits = u16::from_le_bytes(bytes[14..16].try_into().expect("DIB bit depth is present"));
    let compression = u32::from_le_bytes(
        bytes[16..20]
            .try_into()
            .expect("DIB compression is present"),
    );
    if planes != 1 || !matches!(bits, 1 | 4 | 8 | 16 | 24 | 32) {
        return Err(invalid_data("DIB plane count or bit depth is unsupported"));
    }
    if !matches!(compression, 0 | 3 | 6) {
        return Err(invalid_data("DIB compression is unsupported"));
    }
    if compression != 0 && !matches!(bits, 16 | 32) {
        return Err(invalid_data(
            "DIB bitfield compression requires 16-bit or 32-bit pixels",
        ));
    }
    let row_bits = u64::from(width)
        .checked_mul(u64::from(bits))
        .ok_or_else(|| invalid_data("DIB row length overflow"))?;
    let row_bytes = row_bits.div_ceil(32).saturating_mul(4);
    let pixel_bytes = row_bytes
        .checked_mul(u64::from(height))
        .ok_or_else(|| invalid_data("DIB pixel length overflow"))?;
    let color_count = u32::from_le_bytes(
        bytes[32..36]
            .try_into()
            .expect("DIB used-color count is present"),
    );
    let palette_entries = if color_count != 0 {
        u64::from(color_count)
    } else if bits <= 8 {
        1_u64 << bits
    } else {
        0
    };
    let external_masks = if header_size == 40 {
        match compression {
            3 => 12_u64,
            6 => 16_u64,
            _ => 0_u64,
        }
    } else {
        0
    };
    let pixel_offset = u64::try_from(header_size)
        .unwrap_or(u64::MAX)
        .checked_add(external_masks)
        .and_then(|value| value.checked_add(palette_entries.saturating_mul(4)))
        .ok_or_else(|| invalid_data("DIB pixel offset overflow"))?;
    let minimum = pixel_offset
        .checked_add(pixel_bytes)
        .ok_or_else(|| invalid_data("DIB total length overflow"))?;
    if minimum > u64::try_from(bytes.len()).unwrap_or(u64::MAX) {
        return Err(invalid_data("DIB pixel payload is truncated"));
    }
    let declared_image_size = u64::from(u32::from_le_bytes(
        bytes[20..24].try_into().expect("DIB image size is present"),
    ));
    if declared_image_size != 0 && declared_image_size < pixel_bytes {
        return Err(invalid_data("DIB declared image size is too small"));
    }
    if header_size >= 124 {
        let profile_offset = u64::from(u32::from_le_bytes(
            bytes[112..116]
                .try_into()
                .expect("DIB profile offset is present"),
        ));
        let profile_size = u64::from(u32::from_le_bytes(
            bytes[116..120]
                .try_into()
                .expect("DIB profile size is present"),
        ));
        if profile_size != 0 {
            let profile_end = profile_offset
                .checked_add(profile_size)
                .ok_or_else(|| invalid_data("DIB profile length overflow"))?;
            if profile_offset < u64::try_from(header_size).unwrap_or(u64::MAX)
                || profile_end > u64::try_from(bytes.len()).unwrap_or(u64::MAX)
            {
                return Err(invalid_data("DIB color profile is out of bounds"));
            }
        }
    }
    Ok(())
}

fn validate_image_dimensions(width: u32, height: u32) -> io::Result<()> {
    if width == 0 || height == 0 || width > MAX_IMAGE_DIMENSION || height > MAX_IMAGE_DIMENSION {
        return Err(invalid_data(
            "image dimensions exceed the shared clipboard limit",
        ));
    }
    let decoded = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| invalid_data("decoded image size overflow"))?;
    if decoded > u64::try_from(MAX_IMAGE_DECODED_BYTES).unwrap_or(u64::MAX) {
        return Err(invalid_data(
            "decoded image exceeds the shared clipboard limit",
        ));
    }
    Ok(())
}

pub(crate) fn validate_format_set(formats: &[SharedFormatData]) -> io::Result<()> {
    if formats.is_empty() || formats.len() > MAX_SHARED_FORMATS {
        return Err(invalid_data("shared clipboard format count is invalid"));
    }
    let mut kinds = HashSet::with_capacity(formats.len());
    let mut total = 0_usize;
    for format in formats {
        if !kinds.insert(format.kind) {
            return Err(invalid_data("shared clipboard format is duplicated"));
        }
        validate_format_payload(format.kind, &format.bytes)?;
        total = total
            .checked_add(format.bytes.len())
            .ok_or_else(|| invalid_data("shared clipboard total length overflow"))?;
    }
    if total > MAX_SHARED_SNAPSHOT_BYTES {
        return Err(invalid_data(
            "shared clipboard snapshot exceeds its fixed limit",
        ));
    }
    Ok(())
}

fn validate_format_payload(kind: SharedFormatKind, bytes: &[u8]) -> io::Result<()> {
    if bytes.is_empty() && kind != SharedFormatKind::UnicodeText {
        return Err(invalid_data("shared clipboard format is empty"));
    }
    match kind {
        SharedFormatKind::UnicodeText => validate_unicode_bytes(bytes),
        SharedFormatKind::Html => {
            if bytes.len() > MAX_RICH_TEXT_BYTES {
                Err(invalid_data("HTML clipboard payload is too large"))
            } else {
                normalize_html(bytes.to_vec()).map(|_| ())
            }
        }
        SharedFormatKind::Rtf => {
            if bytes.len() > MAX_RICH_TEXT_BYTES {
                Err(invalid_data("RTF clipboard payload is too large"))
            } else {
                normalize_rtf(bytes.to_vec()).map(|_| ())
            }
        }
        SharedFormatKind::Png => normalize_png(bytes).map(|_| ()),
        SharedFormatKind::DibV5 => validate_dib(bytes, true),
        SharedFormatKind::Dib => validate_dib(bytes, false),
    }
}

fn validate_unicode_bytes(bytes: &[u8]) -> io::Result<()> {
    if bytes.len() > MAX_UNICODE_TEXT_BYTES || !bytes.len().is_multiple_of(2) {
        return Err(invalid_data("shared Unicode text length is invalid"));
    }
    if bytes
        .chunks_exact(2)
        .any(|pair| u16::from_le_bytes([pair[0], pair[1]]) == 0)
    {
        return Err(invalid_data("shared Unicode text contains an embedded NUL"));
    }
    Ok(())
}

fn capture_windows_error(
    stage: &'static str,
    format: Option<SharedFormatKind>,
    error: &Error,
) -> io::Error {
    let format = format.map_or_else(String::new, |kind| format!(" format={kind:?}"));
    io::Error::other(format!(
        "stage={stage}{format} error={error} hresult={:#010X}",
        error.code().0.cast_unsigned()
    ))
}

fn capture_data_error(
    stage: &'static str,
    format: Option<SharedFormatKind>,
    error: &io::Error,
) -> io::Error {
    let format = format.map_or_else(String::new, |kind| format!(" format={kind:?}"));
    io::Error::other(format!("stage={stage}{format} error={error}"))
}

fn read_shared_origin(format: u32) -> bool {
    let Ok(handle) = (unsafe { GetClipboardData(format) }) else {
        return false;
    };
    let global = HGLOBAL(handle.0);
    let expected = SHARED_ORIGIN_PREFIX.len() + 16;
    if unsafe { GlobalSize(global) } != expected {
        return false;
    }
    let pointer = unsafe { GlobalLock(global) };
    if pointer.is_null() {
        return false;
    }
    let payload = unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), expected) };
    let valid = payload.starts_with(SHARED_ORIGIN_PREFIX);
    let _ = unsafe { GlobalUnlock(global) };
    valid
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn text_format(units: &[u16]) -> SharedFormatData {
        let mut bytes = Vec::with_capacity(units.len() * 2);
        for unit in units {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        SharedFormatData::new(SharedFormatKind::UnicodeText, bytes.into()).unwrap()
    }

    #[test]
    fn snapshots_preserve_exact_utf16_and_multiple_formats() {
        let text = text_format(&[0x0041, 0xD83D, 0xDE80, 0xD800]);
        let rtf = SharedFormatData::new(
            SharedFormatKind::Rtf,
            Arc::from(b"{\\rtf1 test}\0".as_slice()),
        )
        .unwrap();
        let snapshot =
            SharedClipboardSnapshot::received(ClipboardEventId([7; 16]), 9, Arc::from([text, rtf]))
                .unwrap();
        assert_eq!(snapshot.unicode_units(), 4);
        assert_eq!(snapshot.format_labels(), "文字+RTF");
        assert!(snapshot.origin_payload().starts_with(SHARED_ORIGIN_PREFIX));
        assert_eq!(
            &snapshot.origin_payload()[SHARED_ORIGIN_PREFIX.len()..],
            &[7; 16]
        );
        assert!(
            SharedClipboardSnapshot::received(
                ClipboardEventId([1; 16]),
                0,
                Arc::from([text_format(&[1])]),
            )
            .is_err()
        );
    }

    #[test]
    fn capture_helper_frame_round_trips_and_rejects_length_confusion() {
        let capture = SharedClipboardCapture::Snapshot {
            formats: Arc::from([
                text_format(&[0x4F60, 0x597D, 0xD83D, 0xDE80]),
                SharedFormatData::new(
                    SharedFormatKind::Rtf,
                    Arc::from(b"{\\rtf1 hello}\0".as_slice()),
                )
                .unwrap(),
            ]),
            sequence: 91,
        };
        let frame = encode_capture_helper_result(&capture).unwrap();
        let SharedClipboardCapture::Snapshot { formats, sequence } =
            decode_capture_helper_result(&frame).unwrap()
        else {
            panic!("expected shared capture");
        };
        assert_eq!(sequence, 91);
        assert_eq!(formats.len(), 2);
        assert_eq!(formats[0].kind, SharedFormatKind::UnicodeText);
        assert_eq!(formats[1].kind, SharedFormatKind::Rtf);

        assert!(decode_capture_helper_result(&frame[..frame.len() - 1]).is_err());
        let mut bad = frame;
        bad[10] = 1;
        assert!(decode_capture_helper_result(&bad).is_err());
    }

    #[test]
    fn capture_failure_preserves_stage_format_and_hresult() {
        let error = Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG);
        let message =
            capture_windows_error("read_format", Some(SharedFormatKind::Html), &error).to_string();
        assert!(message.contains("stage=read_format"));
        assert!(message.contains("format=Html"));
        assert!(message.contains("hresult=0x80070057"));
    }

    #[test]
    fn helper_stderr_is_bounded_and_flattened_for_product_logs() {
        let diagnostic = read_bounded_helper_diagnostic(
            std::io::Cursor::new(b"first line\r\nsecond line with extra bytes"),
            24,
        )
        .unwrap();
        assert!(diagnostic.truncated);
        assert_eq!(diagnostic.bytes.len(), 24);
        let message = with_helper_diagnostic("helper failed".to_owned(), &diagnostic);
        assert!(message.contains("helper_stderr=first line second line"));
        assert!(message.ends_with("[truncated]"));
        assert!(!message.contains(['\r', '\n']));
    }

    #[test]
    fn png_and_dib_bounds_reject_malformed_or_excessive_inputs() {
        assert!(normalize_png(b"not-png").is_err());
        let mut dib = vec![0_u8; 40 + 4];
        dib[0..4].copy_from_slice(&40_u32.to_le_bytes());
        dib[4..8].copy_from_slice(&1_i32.to_le_bytes());
        dib[8..12].copy_from_slice(&1_i32.to_le_bytes());
        dib[12..14].copy_from_slice(&1_u16.to_le_bytes());
        dib[14..16].copy_from_slice(&32_u16.to_le_bytes());
        validate_dib(&dib, false).unwrap();
        dib[4..8].copy_from_slice(&20_000_i32.to_le_bytes());
        assert!(validate_dib(&dib, false).is_err());
    }

    #[test]
    fn html_and_rtf_are_normalized_with_strict_boundaries() {
        let template = "Version:1.0\r\nStartHTML:0000000000\r\nEndHTML:0000000000\r\n\r\n";
        let body = b"<html><body>hello</body></html>";
        let start = template.len();
        let end = start + body.len();
        let header = format!("Version:1.0\r\nStartHTML:{start:010}\r\nEndHTML:{end:010}\r\n\r\n");
        assert_eq!(header.len(), start);
        let mut html = header.into_bytes();
        html.extend_from_slice(body);
        html.extend_from_slice(b"ignored allocation tail");
        let normalized = normalize_html(html).unwrap();
        assert_eq!(&normalized[start..end], body);
        assert_eq!(normalized.len(), end + 1);
        assert_eq!(normalized.last(), Some(&0));

        assert_eq!(
            normalize_rtf(b"{\\rtf1 hello}\0tail".to_vec()).unwrap(),
            b"{\\rtf1 hello}\0"
        );
        assert!(normalize_rtf(b"plain text".to_vec()).is_err());
    }

    #[test]
    fn png_crc_and_indexed_dib_palette_are_checked() {
        let png = [
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00,
            0x00, 0xB5, 0x1C, 0x0C, 0x02, 0x00, 0x00, 0x00, 0x0B, 0x49, 0x44, 0x41, 0x54, 0x78,
            0xDA, 0x63, 0xFC, 0xFF, 0x1F, 0x00, 0x02, 0xEB, 0x01, 0xF5, 0x16, 0x15, 0x14, 0x76,
            0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        assert_eq!(normalize_png(&png).unwrap(), png);
        let mut corrupt = png;
        corrupt[45] ^= 1;
        assert!(normalize_png(&corrupt).is_err());

        let mut indexed = vec![0_u8; 40 + 256 * 4 + 4];
        indexed[0..4].copy_from_slice(&40_u32.to_le_bytes());
        indexed[4..8].copy_from_slice(&1_i32.to_le_bytes());
        indexed[8..12].copy_from_slice(&1_i32.to_le_bytes());
        indexed[12..14].copy_from_slice(&1_u16.to_le_bytes());
        indexed[14..16].copy_from_slice(&8_u16.to_le_bytes());
        validate_dib(&indexed, false).unwrap();
        indexed.truncate(44);
        assert!(validate_dib(&indexed, false).is_err());
    }
}
