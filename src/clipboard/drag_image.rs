//! Source-rendered Shell icons. Only bounded BGRA pixels cross the wire, never GDI handles.
use std::{io, path::Path};
use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection,
    DEFAULT_GUI_FONT, DIB_RGB_COLORS, DT_CENTER, DT_END_ELLIPSIS, DT_NOPREFIX, DT_WORDBREAK,
    DeleteDC, DeleteObject, DrawTextW, GetDIBits, GetObjectW, GetStockObject, HBITMAP,
    SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IDataObject};
use windows::Win32::UI::Shell::{
    CLSID_DragDropHelper, IDragSourceHelper, IShellItemImageFactory, SHCreateItemFromParsingName,
    SHDRAGIMAGE, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_USEFILEATTRIBUTES, SHGetFileInfoW,
    SIIGBF_ICONONLY,
};
use windows::Win32::UI::WindowsAndMessaging::{DI_NORMAL, DestroyIcon, DrawIconEx};
use windows_core::PCWSTR;

/// Source returns are a cancellable preview, never a second local Shell Copy.
/// This tiny click-through window owns its pixels independently of OLE's image manager.
pub(super) struct ReturnedDragPreview {
    window: HWND,
    offset: POINT,
}

impl ReturnedDragPreview {
    pub(super) fn create(bytes: &[u8], cursor: POINT) -> io::Result<Self> {
        use windows::Win32::Graphics::Gdi::{AC_SRC_ALPHA, AC_SRC_OVER, BLENDFUNCTION};
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, ULW_ALPHA, UpdateLayeredWindow, WS_EX_LAYERED, WS_EX_NOACTIVATE,
            WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
        };
        let (width, height) = validate(bytes)?;
        let information = info(width, height);
        let mut bits = std::ptr::null_mut();
        let bitmap = Bitmap(
            unsafe {
                CreateDIBSection(
                    None,
                    &raw const information,
                    DIB_RGB_COLORS,
                    &raw mut bits,
                    None,
                    0,
                )
            }
            .map_err(io::Error::other)?,
        );
        if bits.is_null() {
            return Err(io::Error::other("returned drag preview has no pixels"));
        }
        let pixels = unsafe { std::slice::from_raw_parts_mut(bits.cast::<u8>(), bytes.len() - 8) };
        pixels.copy_from_slice(&bytes[8..]);
        for pixel in pixels.chunks_exact_mut(4) {
            let alpha = u16::from(pixel[3]);
            for channel in &mut pixel[..3] {
                *channel = u8::try_from((u16::from(*channel) * alpha + 127) / 255)
                    .expect("premultiplied channel fits");
            }
        }
        let preview = Self {
            window: unsafe {
                CreateWindowExW(
                    WS_EX_LAYERED
                        | WS_EX_TRANSPARENT
                        | WS_EX_NOACTIVATE
                        | WS_EX_TOOLWINDOW
                        | WS_EX_TOPMOST,
                    windows_core::w!("STATIC"),
                    windows_core::w!("Pierdesk Returned Drag Preview"),
                    WS_POPUP,
                    0,
                    0,
                    width,
                    height,
                    None,
                    None,
                    None,
                    None,
                )
            }
            .map_err(io::Error::other)?,
            offset: POINT {
                x: width / 2,
                y: 12.min(height - 1),
            },
        };
        let dc = unsafe { CreateCompatibleDC(None) };
        if dc.0.is_null() {
            return Err(io::Error::last_os_error());
        }
        let previous = unsafe { SelectObject(dc, bitmap.0.into()) };
        let position = POINT {
            x: cursor.x - preview.offset.x,
            y: cursor.y - preview.offset.y,
        };
        let size = SIZE {
            cx: width,
            cy: height,
        };
        let origin = POINT::default();
        let blend = BLENDFUNCTION {
            BlendOp: u8::try_from(AC_SRC_OVER).expect("blend op fits"),
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: u8::try_from(AC_SRC_ALPHA).expect("alpha flag fits"),
        };
        let result = unsafe {
            UpdateLayeredWindow(
                preview.window,
                None,
                Some(&raw const position),
                Some(&raw const size),
                Some(dc),
                Some(&raw const origin),
                COLORREF(0),
                Some(&raw const blend),
                ULW_ALPHA,
            )
        };
        unsafe {
            let _ = SelectObject(dc, previous);
            let _ = DeleteDC(dc);
        }
        result.map_err(io::Error::other)?;
        preview.follow(cursor)?;
        Ok(preview)
    }

    pub(super) fn follow(&self, cursor: POINT) -> io::Result<()> {
        use windows::Win32::UI::WindowsAndMessaging::{
            HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOSIZE, SWP_SHOWWINDOW, SetWindowPos,
        };
        unsafe {
            SetWindowPos(
                self.window,
                Some(HWND_TOPMOST),
                cursor.x - self.offset.x,
                cursor.y - self.offset.y,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOSIZE | SWP_SHOWWINDOW,
            )
        }
        .map_err(io::Error::other)
    }
}

impl Drop for ReturnedDragPreview {
    fn drop(&mut self) {
        let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::DestroyWindow(self.window) };
    }
}

pub(super) const MAX_BYTES: usize = 8 + 256 * 256 * 4;

pub(super) struct Bitmap(HBITMAP);
impl Drop for Bitmap {
    fn drop(&mut self) {
        let _ = unsafe { DeleteObject(self.0.into()) };
    }
}

struct Icon(windows::Win32::UI::WindowsAndMessaging::HICON);
impl Drop for Icon {
    fn drop(&mut self) {
        let _ = unsafe { DestroyIcon(self.0) };
    }
}

fn info(width: i32, height: i32) -> BITMAPINFO {
    BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: u32::try_from(size_of::<BITMAPINFOHEADER>()).expect("header fits"),
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn add_file_name(icon: &[u8], path: &Path, selected_items: usize) -> io::Result<Vec<u8>> {
    use std::os::windows::ffi::OsStrExt;
    let (icon_width, icon_height) = validate(icon)?;
    let width = 220_i32;
    let height = icon_height + 38;
    let information = info(width, height);
    let mut bits = std::ptr::null_mut();
    let bitmap = Bitmap(
        unsafe {
            CreateDIBSection(
                None,
                &raw const information,
                DIB_RGB_COLORS,
                &raw mut bits,
                None,
                0,
            )
        }
        .map_err(io::Error::other)?,
    );
    if bits.is_null() {
        return Err(io::Error::other("labelled drag image has no pixels"));
    }
    let stride = usize::try_from(width * 4).expect("bounded width");
    let icon_stride = usize::try_from(icon_width * 4).expect("bounded icon width");
    let left = usize::try_from((width - icon_width) / 2).expect("icon fits");
    for row in 0..usize::try_from(icon_height).expect("bounded icon height") {
        unsafe {
            std::ptr::copy_nonoverlapping(
                icon[8 + row * icon_stride..].as_ptr(),
                bits.cast::<u8>().add(row * stride + left * 4),
                icon_stride,
            );
        }
    }
    let dc = unsafe { CreateCompatibleDC(None) };
    if dc.0.is_null() {
        return Err(io::Error::last_os_error());
    }
    let previous = unsafe { SelectObject(dc, bitmap.0.into()) };
    let previous_font = unsafe { SelectObject(dc, GetStockObject(DEFAULT_GUI_FONT)) };
    unsafe {
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, COLORREF(0x00ff_ffff));
    }
    let mut name = path
        .file_name()
        .unwrap_or(path.as_os_str())
        .encode_wide()
        .collect::<Vec<_>>();
    if selected_items > 1 {
        name.extend(format!(" 等 {selected_items} 项").encode_utf16());
    }
    let mut rect = RECT {
        left: 2,
        top: icon_height + 2,
        right: width - 2,
        bottom: height,
    };
    unsafe {
        DrawTextW(
            dc,
            &mut name,
            &raw mut rect,
            DT_CENTER | DT_END_ELLIPSIS | DT_WORDBREAK | DT_NOPREFIX,
        );
        let _ = SelectObject(dc, previous_font);
        let _ = SelectObject(dc, previous);
        let _ = DeleteDC(dc);
    }
    let mut labelled = vec![0_u8; 8 + usize::try_from(width * height * 4).expect("bounded image")];
    labelled[..4].copy_from_slice(&width.to_le_bytes());
    labelled[4..8].copy_from_slice(&height.to_le_bytes());
    unsafe {
        std::ptr::copy_nonoverlapping(
            bits.cast::<u8>(),
            labelled[8..].as_mut_ptr(),
            labelled.len() - 8,
        );
    }
    Ok(labelled)
}

pub(super) fn validate(bytes: &[u8]) -> io::Result<(i32, i32)> {
    if bytes.len() < 8 || bytes.len() > MAX_BYTES {
        return Err(io::Error::other("invalid drag image length"));
    }
    let width = i32::from_le_bytes(bytes[..4].try_into().expect("header checked"));
    let height = i32::from_le_bytes(bytes[4..8].try_into().expect("header checked"));
    if !(1..=256).contains(&width)
        || !(1..=256).contains(&height)
        || bytes.len() != 8 + usize::try_from(width * height * 4).expect("bounded dimensions")
    {
        return Err(io::Error::other("invalid drag image geometry"));
    }
    Ok((width, height))
}

pub(crate) fn capture_icon(path: &Path) -> io::Result<Vec<u8>> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .map(|unit| {
            if unit == u16::from(b'/') {
                u16::from(b'\\')
            } else {
                unit
            }
        })
        .chain(Some(0))
        .collect();
    let item: IShellItemImageFactory =
        unsafe { SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None) }
            .map_err(|error| io::Error::other(format!("create Shell image item: {error}")))?;
    let bitmap = Bitmap(
        unsafe { item.GetImage(SIZE { cx: 48, cy: 48 }, SIIGBF_ICONONLY) }
            .map_err(io::Error::other)?,
    );
    let mut description = BITMAP::default();
    if unsafe {
        GetObjectW(
            bitmap.0.into(),
            i32::try_from(size_of::<BITMAP>()).expect("bitmap fits"),
            Some((&raw mut description).cast()),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let (width, height) = (description.bmWidth, description.bmHeight);
    if !(1..=256).contains(&width) || !(1..=256).contains(&height) {
        return Err(io::Error::other("Shell icon exceeds limit"));
    }
    let mut bytes = vec![0; 8 + usize::try_from(width * height * 4).expect("bounded image")];
    bytes[..4].copy_from_slice(&width.to_le_bytes());
    bytes[4..8].copy_from_slice(&height.to_le_bytes());
    let mut information = info(width, height);
    let dc = unsafe { CreateCompatibleDC(None) };
    if dc.0.is_null() {
        return Err(io::Error::last_os_error());
    }
    let copied = unsafe {
        GetDIBits(
            dc,
            bitmap.0,
            0,
            height.cast_unsigned(),
            Some(bytes[8..].as_mut_ptr().cast()),
            &raw mut information,
            DIB_RGB_COLORS,
        )
    };
    let _ = unsafe { DeleteDC(dc) };
    if copied != height {
        return Err(io::Error::other("could not read Shell icon pixels"));
    }
    Ok(bytes)
}

/// Resolves the registered Windows icon for a file extension or folder even when a completed
/// history item's original path no longer exists on this computer.
pub(crate) fn capture_type_icon(path: &Path, is_directory: bool) -> io::Result<Vec<u8>> {
    use std::os::windows::ffi::OsStrExt;
    let wide = path
        .as_os_str()
        .encode_wide()
        .map(|unit| {
            if unit == u16::from(b'/') {
                u16::from(b'\\')
            } else {
                unit
            }
        })
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut shell = SHFILEINFOW::default();
    let attributes = if is_directory {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_NORMAL
    };
    if unsafe {
        SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            attributes,
            Some(&raw mut shell),
            u32::try_from(size_of::<SHFILEINFOW>()).expect("Shell file info fits u32"),
            SHGFI_ICON | SHGFI_LARGEICON | SHGFI_USEFILEATTRIBUTES,
        )
    } == 0
        || shell.hIcon.0.is_null()
    {
        return Err(io::Error::last_os_error());
    }
    let icon = Icon(shell.hIcon);
    let (width, height) = (48_i32, 48_i32);
    let information = info(width, height);
    let mut bits = std::ptr::null_mut();
    let bitmap = Bitmap(
        unsafe {
            CreateDIBSection(
                None,
                &raw const information,
                DIB_RGB_COLORS,
                &raw mut bits,
                None,
                0,
            )
        }
        .map_err(io::Error::other)?,
    );
    if bits.is_null() {
        return Err(io::Error::other("type icon bitmap has no pixels"));
    }
    let dc = unsafe { CreateCompatibleDC(None) };
    if dc.0.is_null() {
        return Err(io::Error::last_os_error());
    }
    let previous = unsafe { SelectObject(dc, bitmap.0.into()) };
    let drawn = unsafe { DrawIconEx(dc, 0, 0, icon.0, width, height, 0, None, DI_NORMAL) };
    unsafe {
        let _ = SelectObject(dc, previous);
        let _ = DeleteDC(dc);
    }
    drawn.map_err(io::Error::other)?;
    let mut bytes = vec![0_u8; 8 + usize::try_from(width * height * 4).expect("bounded icon")];
    bytes[..4].copy_from_slice(&width.to_le_bytes());
    bytes[4..8].copy_from_slice(&height.to_le_bytes());
    unsafe {
        std::ptr::copy_nonoverlapping(bits.cast::<u8>(), bytes[8..].as_mut_ptr(), bytes.len() - 8);
    }
    if !bytes[8..].chunks_exact(4).any(|pixel| pixel[3] != 0) {
        for pixel in bytes[8..].chunks_exact_mut(4) {
            pixel[3] = pixel[..3].iter().copied().max().unwrap_or(0);
        }
    }
    Ok(bytes)
}

pub(super) fn capture(path: &Path, selected_items: usize) -> io::Result<Vec<u8>> {
    // Shell thumbnail/image providers can fail independently of file readability.
    // A registered type icon still gives the user a visible drag without aborting the offer.
    let icon = capture_icon(path).or_else(|_| capture_type_icon(path, path.is_dir()))?;
    let mut bytes = add_file_name(&icon, path, selected_items).unwrap_or(icon);
    let (labelled_width, labelled_height) = validate(&bytes)?;
    let label_start = usize::try_from(labelled_width * labelled_height.saturating_sub(38) * 4)
        .expect("bounded label offset");
    for pixel in bytes[8 + label_start..].chunks_exact_mut(4) {
        if pixel[3] == 0 {
            pixel[3] = pixel[..3].iter().copied().max().unwrap_or(0);
        }
    }
    // Shell and GDI bitmaps are premultiplied; InitializeFromBitmap expects straight alpha.
    for pixel in bytes[8..].chunks_exact_mut(4) {
        let alpha = u32::from(pixel[3]);
        for channel in &mut pixel[..3] {
            *channel = u8::try_from(
                (u32::from(*channel) * 255)
                    .checked_div(alpha)
                    .unwrap_or(0)
                    .min(255),
            )
            .expect("clamped");
        }
    }
    Ok(bytes)
}

pub(super) fn initialize(object: &IDataObject, bytes: &[u8]) -> io::Result<IDragSourceHelper> {
    let (width, height) = validate(bytes)?;
    let information = info(width, height);
    let mut bits = std::ptr::null_mut();
    let bitmap = Bitmap(
        unsafe {
            CreateDIBSection(
                None,
                &raw const information,
                DIB_RGB_COLORS,
                &raw mut bits,
                None,
                0,
            )
        }
        .map_err(io::Error::other)?,
    );
    if bits.is_null() {
        return Err(io::Error::other("drag image has no pixels"));
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bytes[8..].as_ptr(), bits.cast::<u8>(), bytes.len() - 8);
    }
    let image = SHDRAGIMAGE {
        sizeDragImage: SIZE {
            cx: width,
            cy: height,
        },
        ptOffset: POINT {
            x: (width / 2).min(width - 1),
            y: 12.min(height - 1),
        },
        hbmpDragImage: bitmap.0,
        crColorKey: COLORREF(0xffff_ffff),
    };
    let helper: IDragSourceHelper =
        unsafe { CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER) }
            .map_err(io::Error::other)?;
    unsafe { helper.InitializeFromBitmap(&raw const image, object) }.map_err(io::Error::other)?;
    // On success the Shell image manager owns this HBITMAP. Deleting it again when
    // our drag loop ends can destroy a recycled GDI handle on a later crossing.
    // See Chromium OSExchangeDataProviderWin::SetDragImage and Microsoft's DragDropHelper.
    std::mem::forget(bitmap);
    Ok(helper)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn returned_preview_is_visible_click_through_and_recreated_without_clipboard_changes() {
        use windows::Win32::Graphics::Gdi::{
            BitBlt, CAPTUREBLT, CreateCompatibleBitmap, GetDC, GetPixel, ReleaseDC, SRCCOPY,
        };
        use windows::Win32::System::DataExchange::GetClipboardSequenceNumber;
        use windows::Win32::UI::WindowsAndMessaging::{
            GWL_EXSTYLE, GetForegroundWindow, GetWindowLongPtrW, GetWindowRect, IsWindow,
            IsWindowVisible, WS_EX_NOACTIVATE, WS_EX_TRANSPARENT,
        };
        let _dpi = crate::cross_screen::ThreadDpiAwarenessGuard::enter_per_monitor().unwrap();
        let mut bytes = vec![255; 8 + 16 * 16 * 4];
        bytes[..4].copy_from_slice(&16_i32.to_le_bytes());
        bytes[4..8].copy_from_slice(&16_i32.to_le_bytes());
        for pixel in bytes[8..].chunks_exact_mut(4) {
            pixel.copy_from_slice(&[20, 150, 240, 255]);
        }
        let clipboard = unsafe { GetClipboardSequenceNumber() };
        let foreground = unsafe { GetForegroundWindow() };
        for _ in 0..8 {
            let preview = ReturnedDragPreview::create(&bytes, POINT { x: 40, y: 40 }).unwrap();
            assert!(unsafe { IsWindowVisible(preview.window) }.as_bool());
            let flags = unsafe { GetWindowLongPtrW(preview.window, GWL_EXSTYLE) }.cast_unsigned();
            assert_ne!(flags & WS_EX_TRANSPARENT.0 as usize, 0);
            assert_ne!(flags & WS_EX_NOACTIVATE.0 as usize, 0);
            preview.follow(POINT { x: 80, y: 90 }).unwrap();
            let mut rect = RECT::default();
            unsafe { GetWindowRect(preview.window, &raw mut rect) }.unwrap();
            assert_eq!(
                (rect.left, rect.top, rect.right, rect.bottom),
                (72, 78, 88, 94)
            );
            // Verify compositor output, not merely a successful creation or WS_VISIBLE flag.
            let dc = unsafe { GetDC(None) };
            let capture = unsafe { CreateCompatibleDC(Some(dc)) };
            let sample = Bitmap(unsafe { CreateCompatibleBitmap(dc, 1, 1) });
            let selected = unsafe { SelectObject(capture, sample.0.into()) };
            let mut rendered = false;
            let mut observed = COLORREF(0);
            for _ in 0..50 {
                use windows::Win32::UI::WindowsAndMessaging::{
                    DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
                };
                let mut message = MSG::default();
                while unsafe { PeekMessageW(&raw mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                    unsafe {
                        let _ = TranslateMessage(&raw const message);
                        DispatchMessageW(&raw const message);
                    }
                }
                unsafe { BitBlt(capture, 0, 0, 1, 1, Some(dc), 80, 85, SRCCOPY | CAPTUREBLT) }
                    .unwrap();
                observed = unsafe { GetPixel(capture, 0, 0) };
                rendered = observed == COLORREF(0x0014_96f0);
                if rendered {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            unsafe {
                let _ = SelectObject(capture, selected);
                let _ = DeleteDC(capture);
                ReleaseDC(None, dc);
            }
            assert!(
                rendered,
                "returned drag pixels were not visible on screen: {observed:?}"
            );
            assert_eq!(unsafe { GetForegroundWindow() }, foreground);
            let window = preview.window;
            drop(preview);
            assert!(!unsafe { IsWindow(Some(window)) }.as_bool());
        }
        assert_eq!(unsafe { GetClipboardSequenceNumber() }, clipboard);
    }

    #[test]
    fn image_wire_is_bounded_and_exact() {
        let mut bytes = vec![0; 12];
        bytes[..4].copy_from_slice(&1_i32.to_le_bytes());
        bytes[4..8].copy_from_slice(&1_i32.to_le_bytes());
        assert_eq!(validate(&bytes).unwrap(), (1, 1));
        assert!(validate(&bytes[..11]).is_err());
        bytes[..4].copy_from_slice(&257_i32.to_le_bytes());
        assert!(validate(&bytes).is_err());
        bytes[..4].copy_from_slice(&(-1_i32).to_le_bytes());
        assert!(validate(&bytes).is_err());
    }

    #[test]
    fn native_shell_icon_initializes_drag_image_without_reading_file_contents() {
        use std::sync::Arc;
        use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
        struct Apartment;
        impl Drop for Apartment {
            fn drop(&mut self) {
                unsafe { OleUninitialize() };
            }
        }
        unsafe { OleInitialize(None) }.unwrap();
        let _apartment = Apartment;
        let bytes = capture(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
            1,
        )
        .unwrap();
        let (width, height) = validate(&bytes).unwrap();
        assert_eq!(width, 220);
        assert!(height > 48);
        assert!(bytes[8..].chunks_exact(4).any(|pixel| pixel[3] != 0));
        let label_offset = 8 + usize::try_from(width * (height - 34) * 4).unwrap();
        assert!(
            bytes[label_offset..]
                .chunks_exact(4)
                .any(|pixel| pixel[3] != 0)
        );
        let probe = Arc::new(crate::clipboard::probe::ProbeState::quiet());
        let source = Arc::new(crate::clipboard::source::MemorySource::new(
            &b"not read"[..],
        ));
        let object = crate::clipboard::data_object::VirtualFileDataObject::create(
            Arc::from("icon-test.md"),
            source,
            Arc::clone(&probe),
        )
        .unwrap();
        let _bitmap = initialize(&object, &bytes).unwrap();
        assert_eq!(probe.read_calls(), 0);
    }

    #[test]
    fn registered_type_icons_work_without_an_existing_history_path() {
        use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
        struct Apartment;
        impl Drop for Apartment {
            fn drop(&mut self) {
                unsafe { OleUninitialize() };
            }
        }
        unsafe { OleInitialize(None) }.unwrap();
        let _apartment = Apartment;
        for (path, directory) in [
            (Path::new(r"C:\missing\history.xlsx"), false),
            (Path::new(r"C:\missing\notes.txt"), false),
            (Path::new(r"C:\missing\photo.png"), false),
            (Path::new(r"C:\missing\music.mp3"), false),
            (Path::new(r"C:\missing\video.mp4"), false),
            (Path::new(r"C:\missing\folder"), true),
        ] {
            let bytes = capture_type_icon(path, directory).unwrap();
            assert_eq!(validate(&bytes).unwrap(), (48, 48));
            assert!(bytes[8..].chunks_exact(4).any(|pixel| pixel[3] != 0));
        }
    }
}
