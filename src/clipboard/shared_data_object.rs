#![allow(clippy::inline_always, clippy::ref_as_ptr)]

use std::io;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    DATA_S_SAMEFORMATETC, DV_E_DVASPECT, DV_E_DVTARGETDEVICE, DV_E_FORMATETC, DV_E_LINDEX,
    DV_E_TYMED, E_FAIL, E_NOTIMPL, E_POINTER, OLE_E_ADVISENOTSUPPORTED, S_OK,
};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppBGRA, IWICImagingFactory,
    WICBitmapDitherTypeNone, WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnLoad,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CoCreateInstance, DATADIR_GET, DVASPECT_CONTENT, FORMATETC, IAdviseSink,
    IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, STGMEDIUM, TYMED_HGLOBAL,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Ole::{CF_DIB, CF_DIBV5, CF_UNICODETEXT};
use windows::core::{BOOL, Error, HRESULT, PCWSTR, Ref, Result, implement, w};

use super::data_object::{format_etc, hglobal_medium};
use super::format_enum::FormatEnumerator;
use super::probe::ProbeState;
use super::shared_diagnostics::SharedTrace;
use super::shared_protocol::{SharedFormatDescriptor, SharedTextBegin};
#[cfg(test)]
use super::snapshot::{ClipboardEventId, SharedClipboardSnapshot};
use super::snapshot::{
    HTML_FORMAT_NAME, MAX_IMAGE_DECODED_BYTES, MAX_IMAGE_DIMENSION, PNG_FORMAT_NAME,
    RTF_FORMAT_NAME, SHARED_ORIGIN_FORMAT, SHARED_ORIGIN_PREFIX, SharedFormatData,
    SharedFormatKind, validate_format_set,
};
use super::{catch_com_hresult, catch_com_result};

const CAN_UPLOAD_TO_CLOUD_FORMAT: PCWSTR = w!("CanUploadToCloudClipboard");
const CAN_INCLUDE_IN_HISTORY_FORMAT: PCWSTR = w!("CanIncludeInClipboardHistory");
const DIBV5_HEADER_SIZE: usize = 124;
const DIB_BI_BITFIELDS: u32 = 3;
const DIB_LCS_SRGB: u32 = 0x7352_4742;
const DIB_LCS_GM_IMAGES: u32 = 4;

#[derive(Clone, Copy)]
struct RegisteredSharedFormat {
    kind: SharedFormatKind,
    clipboard_id: u16,
}

struct SharedFormats {
    content: Arc<[RegisteredSharedFormat]>,
    origin: u16,
    can_upload_to_cloud: u16,
    can_include_in_history: u16,
}

impl SharedFormats {
    fn register(descriptors: &[SharedFormatDescriptor]) -> Result<Self> {
        let origin = register_format(SHARED_ORIGIN_FORMAT)?;
        let can_upload_to_cloud = register_format(CAN_UPLOAD_TO_CLOUD_FORMAT)?;
        let can_include_in_history = register_format(CAN_INCLUDE_IN_HISTORY_FORMAT)?;
        let mut content = descriptors
            .iter()
            .map(|descriptor| {
                Ok(RegisteredSharedFormat {
                    kind: descriptor.kind,
                    clipboard_id: match descriptor.kind {
                        SharedFormatKind::UnicodeText => CF_UNICODETEXT.0,
                        SharedFormatKind::Html => register_format(HTML_FORMAT_NAME)?,
                        SharedFormatKind::Rtf => register_format(RTF_FORMAT_NAME)?,
                        SharedFormatKind::Png => register_format(PNG_FORMAT_NAME)?,
                        SharedFormatKind::DibV5 => CF_DIBV5.0,
                        SharedFormatKind::Dib => CF_DIB.0,
                    },
                })
            })
            .collect::<Result<Vec<_>>>()?;
        // PNG is common for browsers and capture tools, while many native Windows targets ask
        // only for CF_DIBV5. Advertise a bounded, lazily rendered DIBV5 compatibility view without
        // duplicating the image on the wire.
        let has_png = descriptors
            .iter()
            .any(|descriptor| descriptor.kind == SharedFormatKind::Png);
        let has_native_dib = descriptors.iter().any(|descriptor| {
            matches!(
                descriptor.kind,
                SharedFormatKind::DibV5 | SharedFormatKind::Dib
            )
        });
        if has_png && !has_native_dib {
            content.push(RegisteredSharedFormat {
                kind: SharedFormatKind::DibV5,
                clipboard_id: CF_DIBV5.0,
            });
            content.push(RegisteredSharedFormat {
                kind: SharedFormatKind::Dib,
                clipboard_id: CF_DIB.0,
            });
        } else if descriptors
            .iter()
            .any(|descriptor| descriptor.kind == SharedFormatKind::DibV5)
            && !descriptors
                .iter()
                .any(|descriptor| descriptor.kind == SharedFormatKind::Dib)
        {
            // A BITMAPV5HEADER is a valid extended BITMAPINFO header. Publishing the same
            // self-describing payload as CF_DIB supports older targets that never query DIBV5.
            content.push(RegisteredSharedFormat {
                kind: SharedFormatKind::Dib,
                clipboard_id: CF_DIB.0,
            });
        }
        Ok(Self {
            content: content.into(),
            origin,
            can_upload_to_cloud,
            can_include_in_history,
        })
    }

    fn enumerated(&self) -> Arc<[FORMATETC]> {
        self.content
            .iter()
            .map(|format| format_etc(format.clipboard_id, -1, TYMED_HGLOBAL.0.cast_unsigned()))
            .chain(std::iter::once(format_etc(
                self.origin,
                -1,
                TYMED_HGLOBAL.0.cast_unsigned(),
            )))
            .chain(
                [self.can_upload_to_cloud, self.can_include_in_history]
                    .map(|format| format_etc(format, -1, TYMED_HGLOBAL.0.cast_unsigned())),
            )
            .collect::<Vec<_>>()
            .into()
    }

    fn kind_for(&self, clipboard_id: u16) -> Option<SharedFormatKind> {
        self.content
            .iter()
            .find(|format| format.clipboard_id == clipboard_id)
            .map(|format| format.kind)
    }
}

#[implement(IDataObject)]
pub(crate) struct SharedTextDataObject {
    formats: SharedFormats,
    content: Arc<PendingContent>,
    origin_payload: Arc<[u8]>,
    probe: Arc<ProbeState>,
}

enum PendingContentValue {
    Pending,
    Ready(Arc<[SharedFormatData]>),
    Failed,
}

struct PendingContent {
    trace: SharedTrace,
    reads: AtomicU32,
    queries: AtomicU32,
    descriptors: Arc<[SharedFormatDescriptor]>,
    value: Mutex<PendingContentValue>,
    changed: Condvar,
}

#[derive(Clone)]
pub(crate) struct PendingSharedText {
    state: Arc<PendingContent>,
}

impl PendingSharedText {
    pub(crate) fn complete(&self, formats: Arc<[SharedFormatData]>) -> io::Result<()> {
        validate_format_set(&formats)?;
        if self.state.descriptors.len() != formats.len()
            || self
                .state
                .descriptors
                .iter()
                .zip(formats.iter())
                .any(|(descriptor, format)| {
                    descriptor.kind != format.kind
                        || (descriptor.length != 0
                            && usize::try_from(descriptor.length).ok() != Some(format.bytes.len()))
                })
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ready shared clipboard formats do not match the installed object",
            ));
        }
        let mut value = self
            .state
            .value
            .lock()
            .map_err(|_| io::Error::other("pending shared clipboard was poisoned"))?;
        if !matches!(*value, PendingContentValue::Pending) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "pending shared clipboard is no longer current",
            ));
        }
        *value = PendingContentValue::Ready(formats);
        self.state.changed.notify_all();
        drop(value);
        self.state.trace.mark("object_ready", "state=ready");
        Ok(())
    }

    pub(crate) fn fail(&self) {
        let mut value = self
            .state
            .value
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(*value, PendingContentValue::Pending) {
            *value = PendingContentValue::Failed;
            self.state.changed.notify_all();
            drop(value);
            self.state.trace.mark("object_failed", "state=failed");
        }
    }
}

impl PendingContent {
    fn wait_until_ready(&self, trace: &SharedTrace) -> Result<Arc<[SharedFormatData]>> {
        const FIRST_READ_TIMEOUT: Duration = Duration::from_secs(2);
        self.wait_with_timeout(FIRST_READ_TIMEOUT, trace)
    }

    fn wait_with_timeout(
        &self,
        timeout: Duration,
        trace: &SharedTrace,
    ) -> Result<Arc<[SharedFormatData]>> {
        let wait = trace.stage("getdata_wait");
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        let mut value = self
            .value
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            match &*value {
                PendingContentValue::Ready(formats) => {
                    let formats = Arc::clone(formats);
                    drop(value);
                    wait.finish("ready");
                    return Ok(formats);
                }
                PendingContentValue::Failed => {
                    drop(value);
                    wait.finish("failed");
                    return Err(Error::from_hresult(E_FAIL));
                }
                PendingContentValue::Pending => {}
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                drop(value);
                wait.finish("pending_timeout");
                return Err(Error::from_hresult(E_FAIL));
            }
            let waited = self
                .changed
                .wait_timeout(value, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            value = waited.0;
            // Re-check the protected value even after a timeout: completion may have won
            // the lock at the deadline. A timeout never poisons subsequent GetData calls.
        }
    }
}

impl SharedTextDataObject {
    #[cfg(test)]
    pub(crate) fn create(snapshot: &SharedClipboardSnapshot) -> Result<IDataObject> {
        let begin =
            SharedTextBegin::from_snapshot(snapshot).map_err(|error| io_to_windows(&error))?;
        let (object, pending) = Self::create_pending(&begin)?;
        pending
            .complete(Arc::clone(&snapshot.formats))
            .map_err(|error| io_to_windows(&error))?;
        Ok(object)
    }

    pub(crate) fn create_pending(
        begin: &SharedTextBegin,
    ) -> Result<(IDataObject, PendingSharedText)> {
        Self::create_pending_traced(begin, SharedTrace::new(begin, "data_object"))
    }

    fn create_pending_traced(
        begin: &SharedTextBegin,
        trace: SharedTrace,
    ) -> Result<(IDataObject, PendingSharedText)> {
        let content = Arc::new(PendingContent {
            trace,
            reads: AtomicU32::new(0),
            queries: AtomicU32::new(0),
            descriptors: Arc::clone(&begin.formats),
            value: Mutex::new(PendingContentValue::Pending),
            changed: Condvar::new(),
        });
        let pending = PendingSharedText {
            state: Arc::clone(&content),
        };
        let mut origin_payload = Vec::with_capacity(SHARED_ORIGIN_PREFIX.len() + 16);
        origin_payload.extend_from_slice(SHARED_ORIGIN_PREFIX);
        origin_payload.extend_from_slice(&begin.event_id.0);
        let object: IDataObject = Self {
            formats: SharedFormats::register(&begin.formats)?,
            content,
            origin_payload: origin_payload.into(),
            probe: Arc::new(ProbeState::quiet()),
        }
        .into();
        Ok((object, pending))
    }

    fn validate(&self, format: &FORMATETC) -> HRESULT {
        if !format.ptd.is_null() {
            return DV_E_DVTARGETDEVICE;
        }
        if format.dwAspect != DVASPECT_CONTENT.0 {
            return DV_E_DVASPECT;
        }
        if format.lindex != -1 {
            return DV_E_LINDEX;
        }
        if format.cfFormat != self.formats.origin
            && format.cfFormat != self.formats.can_upload_to_cloud
            && format.cfFormat != self.formats.can_include_in_history
            && self.formats.kind_for(format.cfFormat).is_none()
        {
            return DV_E_FORMATETC;
        }
        if format.tymed & TYMED_HGLOBAL.0.cast_unsigned() == 0 {
            return DV_E_TYMED;
        }
        S_OK
    }

    fn medium_for(&self, format: &FORMATETC) -> Result<STGMEDIUM> {
        // Metadata probes do not consume the bounded content-read budget.
        let Some(kind) = self.formats.kind_for(format.cfFormat) else {
            return self.medium_for_inner(format, &SharedTrace::disabled());
        };
        let read = self.content.reads.fetch_add(1, Ordering::Relaxed);
        let trace = if read < 32 {
            self.content.trace.clone()
        } else {
            SharedTrace::disabled()
        };
        if read == 32 {
            self.content.trace.mark("getdata_suppressed", "limit=32");
        }
        trace.mark(
            "getdata_request",
            &format!("read={} format={kind:?}", read + 1),
        );
        let operation = trace.stage("getdata_total");
        let result = self.medium_for_inner(format, &trace);
        let hresult = result.as_ref().map_or_else(Error::code, |_| S_OK);
        operation.finish(&format!("hresult_{:08x}", hresult.0.cast_unsigned()));
        result
    }

    fn medium_for_inner(&self, format: &FORMATETC, trace: &SharedTrace) -> Result<STGMEDIUM> {
        if format.cfFormat == self.formats.origin {
            return hglobal_medium(&self.origin_payload);
        }
        if format.cfFormat == self.formats.can_upload_to_cloud
            || format.cfFormat == self.formats.can_include_in_history
        {
            return hglobal_medium(&0_u32.to_le_bytes());
        }
        let kind = self
            .formats
            .kind_for(format.cfFormat)
            .ok_or_else(|| Error::from_hresult(DV_E_FORMATETC))?;
        let formats = self.content.wait_until_ready(trace)?;
        let render = trace.stage("getdata_render");
        let payload = formats
            .iter()
            .find(|entry| entry.kind == kind)
            .or_else(|| {
                (kind == SharedFormatKind::DibV5)
                    .then(|| {
                        formats
                            .iter()
                            .find(|entry| entry.kind == SharedFormatKind::Png)
                    })
                    .flatten()
            })
            .or_else(|| {
                (kind == SharedFormatKind::Dib)
                    .then(|| {
                        formats.iter().find(|entry| {
                            matches!(entry.kind, SharedFormatKind::DibV5 | SharedFormatKind::Png)
                        })
                    })
                    .flatten()
            })
            .ok_or_else(|| Error::from_hresult(E_FAIL))?;
        let result = if kind == SharedFormatKind::UnicodeText {
            let mut bytes = Vec::with_capacity(payload.bytes.len() + 2);
            bytes.extend_from_slice(&payload.bytes);
            bytes.extend_from_slice(&0_u16.to_le_bytes());
            hglobal_medium(&bytes)
        } else if matches!(kind, SharedFormatKind::DibV5 | SharedFormatKind::Dib)
            && payload.kind == SharedFormatKind::Png
        {
            hglobal_medium(&png_to_dibv5(&payload.bytes)?)
        } else {
            hglobal_medium(&payload.bytes)
        };
        render.finish(if result.is_ok() { "ok" } else { "failed" });
        result
    }
}

impl IDataObject_Impl for SharedTextDataObject_Impl {
    fn GetData(&self, format: *const FORMATETC) -> Result<STGMEDIUM> {
        catch_com_result(|| {
            if format.is_null() {
                return Err(Error::from_hresult(E_POINTER));
            }
            let format = unsafe { format.read() };
            let validation = self.validate(&format);
            if validation.is_err() && self.content.queries.fetch_add(1, Ordering::Relaxed) < 32 {
                self.content.trace.mark(
                    "getdata_rejected",
                    &format!(
                        "format_id={} hresult={:08x}",
                        format.cfFormat,
                        validation.0.cast_unsigned()
                    ),
                );
            }
            validation.ok()?;
            self.medium_for(&format)
        })
    }

    fn GetDataHere(&self, format: *const FORMATETC, medium: *mut STGMEDIUM) -> Result<()> {
        catch_com_result(|| {
            if format.is_null() || medium.is_null() {
                return Err(Error::from_hresult(E_POINTER));
            }
            Err(Error::from_hresult(E_NOTIMPL))
        })
    }

    fn QueryGetData(&self, format: *const FORMATETC) -> HRESULT {
        catch_com_hresult(|| {
            if format.is_null() {
                return E_POINTER;
            }
            let format = unsafe { format.read() };
            let result = self.validate(&format);
            if self.content.queries.fetch_add(1, Ordering::Relaxed) < 32 {
                self.content.trace.mark(
                    "query_getdata",
                    &format!(
                        "format_id={} hresult={:08x}",
                        format.cfFormat,
                        result.0.cast_unsigned()
                    ),
                );
            }
            result
        })
    }

    fn GetCanonicalFormatEtc(&self, input: *const FORMATETC, output: *mut FORMATETC) -> HRESULT {
        catch_com_hresult(|| {
            if input.is_null() || output.is_null() {
                return E_POINTER;
            }
            unsafe { output.write(FORMATETC::default()) };
            DATA_S_SAMEFORMATETC
        })
    }

    fn SetData(
        &self,
        format: *const FORMATETC,
        medium: *const STGMEDIUM,
        _release: BOOL,
    ) -> Result<()> {
        catch_com_result(|| {
            if format.is_null() || medium.is_null() {
                return Err(Error::from_hresult(E_POINTER));
            }
            Err(Error::from_hresult(E_NOTIMPL))
        })
    }

    fn EnumFormatEtc(&self, direction: u32) -> Result<IEnumFORMATETC> {
        catch_com_result(|| {
            if direction != DATADIR_GET.0.cast_unsigned() {
                return Err(Error::from_hresult(E_NOTIMPL));
            }
            Ok(FormatEnumerator::create(
                self.formats.enumerated(),
                Arc::clone(&self.probe),
            ))
        })
    }

    fn DAdvise(
        &self,
        _format: *const FORMATETC,
        _flags: u32,
        _sink: Ref<IAdviseSink>,
    ) -> Result<u32> {
        catch_com_result(|| Err(Error::from_hresult(OLE_E_ADVISENOTSUPPORTED)))
    }

    fn DUnadvise(&self, _connection: u32) -> Result<()> {
        catch_com_result(|| Err(Error::from_hresult(OLE_E_ADVISENOTSUPPORTED)))
    }

    fn EnumDAdvise(&self) -> Result<IEnumSTATDATA> {
        catch_com_result(|| Err(Error::from_hresult(OLE_E_ADVISENOTSUPPORTED)))
    }
}

fn register_format(name: windows_core::PCWSTR) -> Result<u16> {
    let value = unsafe { RegisterClipboardFormatW(name) };
    if value == 0 {
        return Err(Error::from_thread());
    }
    u16::try_from(value).map_err(|_| Error::from_hresult(DV_E_FORMATETC))
}

pub(crate) fn png_to_dibv5(png: &[u8]) -> Result<Vec<u8>> {
    let factory: IWICImagingFactory =
        unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) }?;
    let stream = unsafe { factory.CreateStream() }?;
    unsafe { stream.InitializeFromMemory(png) }?;
    let decoder = unsafe {
        factory.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnLoad)
    }?;
    let frame = unsafe { decoder.GetFrame(0) }?;
    let mut width = 0_u32;
    let mut height = 0_u32;
    unsafe { frame.GetSize(&raw mut width, &raw mut height) }?;
    if width == 0 || height == 0 || width > MAX_IMAGE_DIMENSION || height > MAX_IMAGE_DIMENSION {
        return Err(Error::new(
            E_FAIL,
            "PNG dimensions exceed the clipboard limit",
        ));
    }
    let stride = width
        .checked_mul(4)
        .ok_or_else(|| Error::new(E_FAIL, "PNG stride overflow"))?;
    let pixel_bytes = usize::try_from(stride)
        .ok()
        .and_then(|stride| stride.checked_mul(usize::try_from(height).ok()?))
        .filter(|length| *length <= MAX_IMAGE_DECODED_BYTES)
        .ok_or_else(|| Error::new(E_FAIL, "decoded PNG exceeds the clipboard limit"))?;
    let converter = unsafe { factory.CreateFormatConverter() }?;
    let pixel_format = GUID_WICPixelFormat32bppBGRA;
    unsafe {
        converter.Initialize(
            &frame,
            &raw const pixel_format,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )
    }?;
    let mut pixels = vec![0_u8; pixel_bytes];
    unsafe { converter.CopyPixels(std::ptr::null(), stride, &mut pixels) }?;

    let mut dib = vec![0_u8; DIBV5_HEADER_SIZE];
    write_u32(&mut dib, 0, 124);
    write_i32(
        &mut dib,
        4,
        i32::try_from(width).map_err(|_| Error::new(E_FAIL, "PNG width overflow"))?,
    );
    write_i32(
        &mut dib,
        8,
        -i32::try_from(height).map_err(|_| Error::new(E_FAIL, "PNG height overflow"))?,
    );
    write_u16(&mut dib, 12, 1);
    write_u16(&mut dib, 14, 32);
    write_u32(&mut dib, 16, DIB_BI_BITFIELDS);
    write_u32(
        &mut dib,
        20,
        u32::try_from(pixel_bytes).map_err(|_| Error::new(E_FAIL, "DIB size overflow"))?,
    );
    write_u32(&mut dib, 40, 0x00FF_0000);
    write_u32(&mut dib, 44, 0x0000_FF00);
    write_u32(&mut dib, 48, 0x0000_00FF);
    write_u32(&mut dib, 52, 0xFF00_0000);
    write_u32(&mut dib, 56, DIB_LCS_SRGB);
    write_u32(&mut dib, 108, DIB_LCS_GM_IMAGES);
    dib.extend_from_slice(&pixels);
    Ok(dib)
}

fn write_u16(target: &mut [u8], offset: usize, value: u16) {
    target[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn write_u32(target: &mut [u8], offset: usize, value: u32) {
    target[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_i32(target: &mut [u8], offset: usize, value: i32) {
    target[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
fn io_to_windows(error: &io::Error) -> Error {
    Error::new(E_FAIL, error.to_string())
}

#[cfg(test)]
mod tests {
    use windows::Win32::Foundation::{DV_E_FORMATETC, S_OK};
    use windows::Win32::System::Com::{FORMATETC, TYMED_HGLOBAL};
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    use windows::Win32::System::Ole::{OleInitialize, OleUninitialize, ReleaseStgMedium};

    use super::*;

    fn text_format(units: &[u16]) -> SharedFormatData {
        let mut bytes = Vec::with_capacity(units.len() * 2);
        for unit in units {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        SharedFormatData::new(SharedFormatKind::UnicodeText, bytes.into()).unwrap()
    }

    fn snapshot() -> SharedClipboardSnapshot {
        SharedClipboardSnapshot::received(
            ClipboardEventId([9; 16]),
            1,
            Arc::from([
                text_format(&[0x4F60, 0x597D]),
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
    fn object_exposes_all_formats_and_unicode_has_one_terminator() {
        let object = SharedTextDataObject::create(&snapshot()).unwrap();
        let format = FORMATETC {
            cfFormat: CF_UNICODETEXT.0,
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0.cast_unsigned(),
            ..Default::default()
        };
        assert_eq!(unsafe { object.QueryGetData(&raw const format) }, S_OK);
        let mut medium = unsafe { object.GetData(&raw const format) }.unwrap();
        let global = unsafe { medium.u.hGlobal };
        assert_eq!(unsafe { GlobalSize(global) }, 6);
        let pointer = unsafe { GlobalLock(global) }.cast::<u16>();
        assert!(!pointer.is_null());
        let units = unsafe { std::slice::from_raw_parts(pointer, 3) };
        assert_eq!(units, [0x4F60, 0x597D, 0]);
        let _ = unsafe { GlobalUnlock(global) };
        unsafe { ReleaseStgMedium(&raw mut medium) };

        let rtf = register_format(RTF_FORMAT_NAME).unwrap();
        let rtf_format = FORMATETC {
            cfFormat: rtf,
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0.cast_unsigned(),
            ..Default::default()
        };
        assert_eq!(unsafe { object.QueryGetData(&raw const rtf_format) }, S_OK);
    }

    #[test]
    fn object_rejects_unadvertised_formats() {
        let object = SharedTextDataObject::create(&snapshot()).unwrap();
        let format = FORMATETC {
            cfFormat: 1,
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0.cast_unsigned(),
            ..Default::default()
        };
        assert_eq!(
            unsafe { object.QueryGetData(&raw const format) },
            DV_E_FORMATETC
        );
    }

    #[test]
    fn pending_timeout_is_logged_and_later_completion_remains_readable() {
        let snapshot = snapshot();
        let begin = SharedTextBegin::from_snapshot(&snapshot).unwrap();
        let (sender, receiver) = std::sync::mpsc::sync_channel(64);
        let trace = SharedTrace::for_test(sender);
        let (object, pending) =
            SharedTextDataObject::create_pending_traced(&begin, trace.clone()).unwrap();
        let format = format_etc(CF_UNICODETEXT.0, -1, TYMED_HGLOBAL.0.cast_unsigned());
        let wait_started = Instant::now();
        assert_eq!(
            unsafe { object.GetData(&raw const format) }
                .err()
                .expect("pending GetData must time out")
                .code(),
            E_FAIL
        );
        assert!(wait_started.elapsed() >= Duration::from_secs(2));
        pending.complete(Arc::clone(&snapshot.formats)).unwrap();
        let mut medium = unsafe { object.GetData(&raw const format) }.unwrap();
        unsafe { ReleaseStgMedium(&raw mut medium) };
        // Already-ready data wins even when the deadline has just expired.
        assert!(
            pending
                .state
                .wait_with_timeout(Duration::ZERO, &trace)
                .is_ok()
        );
        let records = receiver.try_iter().collect::<Vec<_>>().join("\n");
        assert!(records.contains("outcome=pending_timeout"));
        assert!(records.contains("outcome=hresult_80004005"));
        assert!(records.contains("phase=object_ready"));
        assert!(records.contains("phase=getdata_render"));
        assert!(records.contains("outcome=hresult_00000000"));
        assert!(!records.contains("hello"));
    }

    #[test]
    fn repeated_content_reads_and_format_probes_have_bounded_logs() {
        let snapshot = snapshot();
        let begin = SharedTextBegin::from_snapshot(&snapshot).unwrap();
        let (sender, receiver) = std::sync::mpsc::sync_channel(512);
        let (object, pending) =
            SharedTextDataObject::create_pending_traced(&begin, SharedTrace::for_test(sender))
                .unwrap();
        pending.complete(Arc::clone(&snapshot.formats)).unwrap();
        let format = format_etc(CF_UNICODETEXT.0, -1, TYMED_HGLOBAL.0.cast_unsigned());
        for _ in 0..40 {
            assert_eq!(unsafe { object.QueryGetData(&raw const format) }, S_OK);
            let mut medium = unsafe { object.GetData(&raw const format) }.unwrap();
            unsafe { ReleaseStgMedium(&raw mut medium) };
        }
        let records = receiver.try_iter().collect::<Vec<_>>();
        assert_eq!(
            records
                .iter()
                .filter(|line| line.contains("phase=getdata_request "))
                .count(),
            32
        );
        assert_eq!(
            records
                .iter()
                .filter(|line| line.contains("phase=query_getdata "))
                .count(),
            32
        );
        assert_eq!(
            records
                .iter()
                .filter(|line| line.contains("phase=getdata_suppressed "))
                .count(),
            1
        );
    }

    #[test]
    fn pending_object_waits_for_atomic_multi_format_completion() {
        let snapshot = snapshot();
        let begin = SharedTextBegin::from_snapshot(&snapshot).unwrap();
        let (object, pending) = SharedTextDataObject::create_pending(&begin).unwrap();
        let formats = Arc::clone(&snapshot.formats);
        let completer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            pending.complete(formats).unwrap();
        });
        let format = FORMATETC {
            cfFormat: CF_UNICODETEXT.0,
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0.cast_unsigned(),
            ..Default::default()
        };
        let mut medium = unsafe { object.GetData(&raw const format) }.unwrap();
        completer.join().unwrap();
        assert_eq!(unsafe { GlobalSize(medium.u.hGlobal) }, 6);
        unsafe { ReleaseStgMedium(&raw mut medium) };
    }

    #[test]
    fn failed_pending_object_does_not_expose_any_content() {
        let snapshot = snapshot();
        let begin = SharedTextBegin::from_snapshot(&snapshot).unwrap();
        let (object, pending) = SharedTextDataObject::create_pending(&begin).unwrap();
        pending.fail();
        let format = FORMATETC {
            cfFormat: CF_UNICODETEXT.0,
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0.cast_unsigned(),
            ..Default::default()
        };
        assert!(unsafe { object.GetData(&raw const format) }.is_err());
    }

    #[test]
    fn png_object_lazily_exposes_native_dibv5_compatibility() {
        const ONE_PIXEL_PNG: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00,
            0x00, 0xB5, 0x1C, 0x0C, 0x02, 0x00, 0x00, 0x00, 0x0B, 0x49, 0x44, 0x41, 0x54, 0x78,
            0xDA, 0x63, 0xFC, 0xFF, 0x1F, 0x00, 0x02, 0xEB, 0x01, 0xF5, 0x16, 0x15, 0x14, 0x76,
            0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        unsafe { OleInitialize(None) }.unwrap();
        let snapshot = SharedClipboardSnapshot::received(
            ClipboardEventId([3; 16]),
            1,
            Arc::from([
                SharedFormatData::new(SharedFormatKind::Png, Arc::from(ONE_PIXEL_PNG)).unwrap(),
            ]),
        )
        .unwrap();
        let object = SharedTextDataObject::create(&snapshot).unwrap();
        let format = FORMATETC {
            cfFormat: CF_DIBV5.0,
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0.cast_unsigned(),
            ..Default::default()
        };
        assert_eq!(unsafe { object.QueryGetData(&raw const format) }, S_OK);
        let dib_format = FORMATETC {
            cfFormat: CF_DIB.0,
            ..format
        };
        assert_eq!(unsafe { object.QueryGetData(&raw const dib_format) }, S_OK);
        let mut medium = unsafe { object.GetData(&raw const format) }.unwrap();
        let global = unsafe { medium.u.hGlobal };
        assert_eq!(unsafe { GlobalSize(global) }, 128);
        let pointer = unsafe { GlobalLock(global) }.cast::<u8>();
        assert!(!pointer.is_null());
        let bytes = unsafe { std::slice::from_raw_parts(pointer, 128) };
        assert_eq!(&bytes[0..4], &124_u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &1_i32.to_le_bytes());
        assert_eq!(&bytes[8..12], &(-1_i32).to_le_bytes());
        let _ = unsafe { GlobalUnlock(global) };
        unsafe { ReleaseStgMedium(&raw mut medium) };
        unsafe { OleUninitialize() };
    }
}
