use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use webview2_com::Microsoft::Web::WebView2::Win32::{
    COREWEBVIEW2_BOUNDS_MODE_USE_RAW_PIXELS, COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC,
    CreateCoreWebView2EnvironmentWithOptions, ICoreWebView2, ICoreWebView2Controller,
    ICoreWebView2Controller3, ICoreWebView2EnvironmentOptions, ICoreWebView2Settings3,
};
use webview2_com::{
    CoTaskMemPWSTR, CreateCoreWebView2ControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler, WebMessageReceivedEventHandler,
};
use windows::Win32::Foundation::{
    CloseHandle, E_POINTER, GlobalFree, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, RECT,
    WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleBitmap,
    CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, HBITMAP, HDC,
    HGDIOBJ, ReleaseDC, SRCCOPY, SelectObject,
};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatPng, GUID_WICPixelFormat32bppBGRA,
    IWICImagingFactory, WICBitmapEncoderNoCache,
};
use windows::Win32::System::Com::StructuredStorage::{CreateStreamOnHGlobal, GetHGlobalFromStream};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoUninitialize, STATFLAG_NONAME, STATSTG,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_DIBV5;
use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
use windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{FileSaveDialog, IFileSaveDialog, SIGDN_FILESYSPATH};
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GWLP_USERDATA,
    GetClientRect, GetMessageW, GetSystemMetrics, GetWindowLongPtrW, GetWindowRect, HTBOTTOMRIGHT,
    HTCAPTION, HWND_TOPMOST, IDC_ARROW, KillTimer, LoadCursorW, MSG, PostMessageW, RegisterClassW,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_SHOW,
    SWP_NOZORDER, SWP_SHOWWINDOW, SetForegroundWindow, SetTimer, SetWindowLongPtrW, SetWindowPos,
    ShowWindow, TranslateMessage, UnregisterClassW, WM_APP, WM_CLOSE, WM_NCCREATE, WM_NCHITTEST,
    WM_NCLBUTTONDOWN, WM_SIZE, WM_TIMER, WNDCLASSW, WS_CLIPCHILDREN, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP,
};
use windows_core::{Interface as _, PCWSTR, PWSTR, w};

use super::SelectionGuard;
use super::candidates::{self, Detector, Query, Target};

const CLASS: PCWSTR = w!("PierdeskScreenshotWindow");
const DISPATCH: u32 = WM_APP + 70;
pub(super) const ACTIVATE: u32 = WM_APP + 71;
const MAX_PNG_BYTES: usize = 64 * 1024 * 1024;
const MAX_PIXELS: usize = 64 * 1024 * 1024;
const HTML: &str = include_str!("../../assets/screenshot/index.html");
const SCRIPT: &str = include_str!("../../assets/screenshot/editor.js");

struct Apartment;
struct ParentProcess(HANDLE);
impl Drop for ParentProcess {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

struct Host {
    controller: ICoreWebView2Controller,
    webview: ICoreWebView2,
    token: i64,
}
impl Drop for Host {
    fn drop(&mut self) {
        unsafe {
            let _ = self.webview.remove_WebMessageReceived(self.token);
            let _ = self.controller.Close();
        }
    }
}
struct State {
    pixels: RefCell<Option<Arc<Vec<u8>>>>,
    targets: RefCell<Vec<Target>>,
    detector: Detector,
    host: RefCell<Option<Host>>,
    selection: RefCell<Option<SelectionGuard>>,
    capture: RefCell<Option<String>>,
    pending: RefCell<VecDeque<String>>,
    shown: Cell<bool>,
    present: bool,
    pinned: Cell<bool>,
    done: Cell<bool>,
    bounds: Cell<RECT>,
    language: RefCell<String>,
    started: Cell<std::time::Instant>,
    ready: Cell<bool>,
    awaiting_activation: Cell<bool>,
    latency_log: Option<std::path::PathBuf>,
    parent: Option<ParentProcess>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Message {
    action: String,
    #[serde(default)]
    id: u32,
    #[serde(default)]
    image: String,
    #[serde(default)]
    x: i32,
    #[serde(default)]
    y: i32,
    #[serde(default)]
    width: i32,
    #[serde(default)]
    height: i32,
}

pub(super) fn run(language: &str, selection: SelectionGuard) -> io::Result<()> {
    run_inner(language, Some(selection), None)
}

pub(super) fn run_warm(parent_id: u32) -> io::Result<()> {
    let parent =
        ParentProcess(unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, parent_id) }.map_err(win)?);
    run_inner("en", None, Some(parent))
}

fn prepare_capture() -> io::Result<(RECT, String, Arc<Vec<u8>>)> {
    let (bounds, pixels) = capture_desktop()?;
    let png = encode_png(
        u32::try_from(bounds.right - bounds.left).map_err(io::Error::other)?,
        u32::try_from(bounds.bottom - bounds.top).map_err(io::Error::other)?,
        &pixels,
    )?;
    Ok((bounds, STANDARD.encode(png), Arc::new(pixels)))
}

#[allow(clippy::too_many_lines)] // Keep COM/window lifetime and cleanup in one scope.
fn run_inner(
    language: &str,
    selection: Option<SelectionGuard>,
    parent: Option<ParentProcess>,
) -> io::Result<()> {
    let warm = parent.is_some();
    let started = std::time::Instant::now();
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
        .ok()
        .map_err(win)?;
    let _apartment = Apartment;
    let _dpi = crate::cross_screen::ThreadDpiAwarenessGuard::enter_per_monitor()?;
    let (bounds, capture, pixels) = if warm {
        (
            RECT {
                right: 1,
                bottom: 1,
                ..Default::default()
            },
            None,
            None,
        )
    } else {
        let (bounds, capture, pixels) = prepare_capture()?;
        (bounds, Some(capture), Some(pixels))
    };
    let root = crate::device_store::DeviceStore::current_user()?;
    let mut state = Box::new(State {
        pixels: RefCell::new(pixels),
        targets: RefCell::new(if warm {
            Vec::new()
        } else {
            candidates::snapshot(bounds)
        }),
        detector: Detector::new(),
        host: RefCell::new(None),
        selection: RefCell::new(selection),
        capture: RefCell::new(capture),
        pending: RefCell::new(VecDeque::new()),
        shown: Cell::new(false),
        present: true,
        pinned: Cell::new(false),
        done: Cell::new(false),
        bounds: Cell::new(bounds),
        language: RefCell::new(language.into()),
        started: Cell::new(started),
        ready: Cell::new(false),
        awaiting_activation: Cell::new(warm),
        latency_log: Some(root.root().join("logs").join("screenshot.log")),
        parent,
    });
    let instance = HINSTANCE(unsafe { GetModuleHandleW(None) }.map_err(win)?.0);
    let class = WNDCLASSW {
        lpfnWndProc: Some(procedure),
        hInstance: instance,
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.map_err(win)?,
        lpszClassName: CLASS,
        ..Default::default()
    };
    if unsafe { RegisterClassW(&raw const class) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let window = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            CLASS,
            w!("Pierdesk Screenshot"),
            WS_POPUP | WS_CLIPCHILDREN,
            bounds.left,
            bounds.top,
            bounds.right - bounds.left,
            bounds.bottom - bounds.top,
            None,
            None,
            Some(instance),
            Some((&raw mut *state).cast()),
        )
    }
    .map_err(win)?;
    let result = (|| {
        let host = create_webview(window, &root.root().join("ScreenshotWebView2"))?;
        let html = wide(&HTML.replace("/*__EDITOR__*/", SCRIPT));
        unsafe { host.webview.NavigateToString(PCWSTR(html.as_ptr())) }.map_err(win)?;
        *state.host.borrow_mut() = Some(host);
        unsafe { SetTimer(Some(window), 1, 1000, None) };
        while !state.done.get() {
            let mut message = MSG::default();
            let status = unsafe { GetMessageW(&raw mut message, None, 0, 0) }.0;
            if status == -1 {
                return Err(io::Error::last_os_error());
            }
            if status == 0 {
                break;
            }
            unsafe {
                let _ = TranslateMessage(&raw const message);
                DispatchMessageW(&raw const message);
            }
        }
        if !state.shown.get() {
            return Err(io::Error::other(
                "Screenshot editor initialization timed out",
            ));
        }
        Ok(())
    })();
    state.host.borrow_mut().take();
    unsafe {
        let _ = KillTimer(Some(window), 1);
        let _ = DestroyWindow(window);
        let _ = UnregisterClassW(CLASS, Some(instance));
    }
    result
}

impl State {
    fn send_capture(&self) -> io::Result<()> {
        if let Some(image) = self.capture.borrow_mut().take() {
            let bounds = self.bounds.get();
            self.post(&serde_json::json!({"type":"capture", "image":format!("data:image/png;base64,{image}"), "language":*self.language.borrow(), "width":bounds.right-bounds.left, "height":bounds.bottom-bounds.top, "windows":*self.targets.borrow()}))?;
        }
        Ok(())
    }
    fn activate(&self, window: HWND, english: bool) -> io::Result<()> {
        if !self.awaiting_activation.get() {
            return Ok(());
        }
        let Some(selection) = SelectionGuard::acquire()? else {
            return Ok(());
        };
        self.started.set(std::time::Instant::now());
        let (bounds, capture, pixels) = prepare_capture()?;
        *self.pixels.borrow_mut() = Some(pixels);
        *self.targets.borrow_mut() = candidates::snapshot(bounds);
        self.bounds.set(bounds);
        *self.capture.borrow_mut() = Some(capture);
        *self.selection.borrow_mut() = Some(selection);
        *self.language.borrow_mut() = if english { "en" } else { "zh-CN" }.into();
        self.awaiting_activation.set(false);
        if let Some(log) = &self.latency_log {
            let _ = crate::logging::append(
                log,
                &format!(
                    "screenshot_capture_ready elapsed_ms={} prewarmed=true",
                    self.started.get().elapsed().as_millis()
                ),
            );
        }
        unsafe {
            SetWindowPos(
                window,
                None,
                bounds.left,
                bounds.top,
                bounds.right - bounds.left,
                bounds.bottom - bounds.top,
                SWP_NOZORDER,
            )
        }
        .map_err(win)?;
        if self.ready.get() {
            self.send_capture()?;
        }
        Ok(())
    }
    fn post(&self, value: &serde_json::Value) -> io::Result<()> {
        let json = wide(&value.to_string());
        if let Some(host) = self.host.borrow().as_ref() {
            unsafe { host.webview.PostWebMessageAsJson(PCWSTR(json.as_ptr())) }.map_err(win)?;
        }
        Ok(())
    }
    #[allow(clippy::too_many_lines)] // Keep export ownership and pin transition together.
    fn command(&self, window: HWND, message: &str) -> io::Result<()> {
        if message.len() > MAX_PNG_BYTES * 4 / 3 + 4096 {
            return Err(io::Error::other("Screenshot too large"));
        }
        let message: Message = serde_json::from_str(message).map_err(io::Error::other)?;
        let bounds = self.bounds.get();
        match message.action.as_str() {
            "detect" if self.shown.get() && !self.pinned.get() => {
                if let Some(target) = self
                    .targets
                    .borrow()
                    .iter()
                    .find(|target| target.region.contains(message.x, message.y))
                    .copied()
                {
                    self.detector.query(Query {
                        pixels: self.pixels.borrow().clone(),
                        id: message.id,
                        x: message.x,
                        y: message.y,
                        target,
                        bounds,
                    });
                }
            }
            "ready" => {
                self.ready.set(true);
                self.send_capture()?;
            }
            "shown" if !self.awaiting_activation.get() && !self.shown.replace(true) => unsafe {
                SetTimer(Some(window), 1, 50, None);
                if let Some(log) = &self.latency_log {
                    let _ = crate::logging::append(
                        log,
                        &format!(
                            "screenshot_editor_ready elapsed_ms={}",
                            self.started.get().elapsed().as_millis()
                        ),
                    );
                }
                if self.present {
                    let _ = ShowWindow(window, SW_SHOW);
                    let _ = SetForegroundWindow(window);
                    if let Some(host) = self.host.borrow().as_ref() {
                        let _ = host
                            .controller
                            .MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC);
                    }
                }
            },
            "cancel" => self.done.set(true),
            "complete" | "save" | "pin" => {
                let png = decode_image(&message.image)?;
                let dib = crate::clipboard::png_to_dibv5(&png).map_err(win)?;
                let image_width =
                    i32::from_le_bytes(dib[4..8].try_into().map_err(io::Error::other)?);
                let image_height =
                    i32::from_le_bytes(dib[8..12].try_into().map_err(io::Error::other)?)
                        .unsigned_abs();
                if message.action == "save" && !save_png(window, &png)? {
                    self.post(&serde_json::json!({"type":"idle"}))?;
                    return Ok(());
                }
                write_clipboard(window, &png, &dib)?;
                if message.action == "pin" {
                    self.pixels.borrow_mut().take();
                    if message.width <= 0
                        || message.height <= 0
                        || message.width != image_width
                        || u32::try_from(message.height).ok() != Some(image_height)
                    {
                        return Err(io::Error::other("Invalid pin dimensions"));
                    }
                    let width = message
                        .width
                        .clamp(160, (bounds.right - bounds.left).max(160));
                    let height = message
                        .height
                        .clamp(100, (bounds.bottom - bounds.top).max(100));
                    self.pinned.set(true);
                    self.post(&serde_json::json!({"type":"pinned", "image":message.image}))?;
                    unsafe {
                        SetWindowPos(
                            window,
                            Some(HWND_TOPMOST),
                            bounds.left + message.x.clamp(0, bounds.right - bounds.left - width),
                            bounds.top + message.y.clamp(0, bounds.bottom - bounds.top - height),
                            width,
                            height,
                            SWP_SHOWWINDOW,
                        )
                    }
                    .map_err(win)?;
                    self.selection.borrow_mut().take();
                } else if self.pinned.get() {
                    self.post(&serde_json::json!({"type":"idle"}))?;
                } else {
                    self.done.set(true);
                }
            }
            "drag" if self.pinned.get() => unsafe {
                let _ = ReleaseCapture();
                let _ = PostMessageW(
                    Some(window),
                    WM_NCLBUTTONDOWN,
                    WPARAM(HTCAPTION as usize),
                    LPARAM(0),
                );
            },
            "zoom" if self.pinned.get() => {
                let mut rect = RECT::default();
                unsafe { GetWindowRect(window, &raw mut rect) }.map_err(win)?;
                let width = message.width.clamp(160, bounds.right - bounds.left);
                let height = message.height.clamp(100, bounds.bottom - bounds.top);
                unsafe {
                    SetWindowPos(
                        window,
                        None,
                        rect.left,
                        rect.top,
                        width,
                        height,
                        SWP_NOZORDER,
                    )
                }
                .map_err(win)?;
            }
            _ => {}
        }
        Ok(())
    }
}

fn decode_image(value: &str) -> io::Result<Vec<u8>> {
    let encoded = value
        .strip_prefix("data:image/png;base64,")
        .ok_or_else(|| io::Error::other("Expected a PNG image"))?;
    if encoded.len() > MAX_PNG_BYTES * 4 / 3 + 4 {
        return Err(io::Error::other("Screenshot too large"));
    }
    let png = STANDARD.decode(encoded).map_err(io::Error::other)?;
    crate::clipboard::normalize_png(&png)
}

fn create_webview(window: HWND, root: &Path) -> io::Result<Host> {
    std::fs::create_dir_all(root)?;
    let root = wide(&root.to_string_lossy());
    let (tx, rx) = mpsc::channel();
    CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| {
            unsafe {
                CreateCoreWebView2EnvironmentWithOptions(
                    PCWSTR::null(),
                    PCWSTR(root.as_ptr()),
                    None::<&ICoreWebView2EnvironmentOptions>,
                    &handler,
                )
            }
            .map_err(webview2_com::Error::WindowsError)
        }),
        Box::new(move |result, environment| {
            result?;
            tx.send(environment.ok_or_else(|| windows_core::Error::from(E_POINTER)))
                .map_err(|_| windows_core::Error::from(E_POINTER))?;
            Ok(())
        }),
    )
    .map_err(io::Error::other)?;
    let environment = rx.recv().map_err(io::Error::other)?.map_err(win)?;
    let (tx, rx) = mpsc::channel();
    CreateCoreWebView2ControllerCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| {
            unsafe { environment.CreateCoreWebView2Controller(window, &handler) }
                .map_err(webview2_com::Error::WindowsError)
        }),
        Box::new(move |result, controller| {
            result?;
            tx.send(controller.ok_or_else(|| windows_core::Error::from(E_POINTER)))
                .map_err(|_| windows_core::Error::from(E_POINTER))?;
            Ok(())
        }),
    )
    .map_err(io::Error::other)?;
    let controller = rx.recv().map_err(io::Error::other)?.map_err(win)?;
    if let Ok(raw) = controller.cast::<ICoreWebView2Controller3>() {
        unsafe { raw.SetBoundsMode(COREWEBVIEW2_BOUNDS_MODE_USE_RAW_PIXELS) }.map_err(win)?;
    }
    resize(&controller, window)?;
    unsafe { controller.SetIsVisible(true) }.map_err(win)?;
    let webview = unsafe { controller.CoreWebView2() }.map_err(win)?;
    let settings = unsafe { webview.Settings() }.map_err(win)?;
    unsafe {
        settings
            .SetAreDefaultContextMenusEnabled(false)
            .map_err(win)?;
        settings.SetAreDevToolsEnabled(false).map_err(win)?;
        settings.SetIsStatusBarEnabled(false).map_err(win)?;
        settings.SetIsZoomControlEnabled(false).map_err(win)?;
        settings
            .SetAreDefaultScriptDialogsEnabled(false)
            .map_err(win)?;
        settings.SetAreHostObjectsAllowed(false).map_err(win)?;
    }
    if let Ok(settings) = settings.cast::<ICoreWebView2Settings3>() {
        unsafe { settings.SetAreBrowserAcceleratorKeysEnabled(false) }.map_err(win)?;
    }
    let mut token = 0;
    unsafe {
        webview.add_WebMessageReceived(
            &WebMessageReceivedEventHandler::create(Box::new(move |_, args| {
                if let Some(args) = args {
                    let mut message = PWSTR::null();
                    args.WebMessageAsJson(&raw mut message)?;
                    let message = CoTaskMemPWSTR::from(message).to_string();
                    let pointer = GetWindowLongPtrW(window, GWLP_USERDATA) as *const State;
                    if !pointer.is_null() && message.len() <= MAX_PNG_BYTES * 4 / 3 + 4096 {
                        let state = &*pointer;
                        if state.pending.borrow().len() < 8 {
                            state.pending.borrow_mut().push_back(message);
                            let _ = PostMessageW(Some(window), DISPATCH, WPARAM(0), LPARAM(0));
                        }
                    }
                }
                Ok(())
            })),
            &raw mut token,
        )
    }
    .map_err(win)?;
    Ok(Host {
        controller,
        webview,
        token,
    })
}

fn resize(controller: &ICoreWebView2Controller, window: HWND) -> io::Result<()> {
    let mut rect = RECT::default();
    unsafe { GetClientRect(window, &raw mut rect) }.map_err(win)?;
    unsafe { controller.SetBounds(rect) }.map_err(win)
}

unsafe extern "system" fn procedure(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = lparam.0 as *const CREATESTRUCTW;
        unsafe {
            SetWindowLongPtrW(window, GWLP_USERDATA, (*create).lpCreateParams as isize);
        }
    }
    let pointer = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *const State;
    if !pointer.is_null() {
        let state = unsafe { &*pointer };
        match message {
            ACTIVATE => {
                if state.activate(window, wparam.0 != 0).is_err() {
                    state.done.set(true);
                }
                return LRESULT(0);
            }
            DISPATCH => {
                loop {
                    let next = state.pending.borrow_mut().pop_front();
                    let Some(next) = next else {
                        break;
                    };
                    if let Err(error) = state.command(window, &next) {
                        let _ = state
                            .post(&serde_json::json!({"type":"error","message":error.to_string()}));
                    }
                    if state.done.get() {
                        break;
                    }
                }
                return LRESULT(0);
            }
            WM_SIZE => {
                if let Some(host) = state.host.borrow().as_ref() {
                    let _ = resize(&host.controller, window);
                }
            }
            WM_CLOSE => {
                state.done.set(true);
                return LRESULT(0);
            }
            WM_TIMER
                if state.parent.as_ref().is_some_and(
                    |parent| unsafe { WaitForSingleObject(parent.0, 0) } == WAIT_OBJECT_0,
                ) =>
            {
                state.done.set(true);
            }
            WM_TIMER
                if !state.awaiting_activation.get()
                    && !state.shown.get()
                    && state.started.get().elapsed().as_secs() > 20 =>
            {
                state.done.set(true);
            }
            WM_TIMER => {
                if let Some(reply) = state.detector.poll() {
                    let _ = state.post(&serde_json::json!({"type":"candidates", "id":reply.id, "regions":reply.regions, "window":reply.window, "visual":reply.visual}));
                }
            }
            WM_NCHITTEST if state.pinned.get() => {
                let mut rect = RECT::default();
                let _ = unsafe { GetWindowRect(window, &raw mut rect) };
                let bytes = lparam.0.to_le_bytes();
                let x = i32::from(i16::from_le_bytes([bytes[0], bytes[1]]));
                let y = i32::from(i16::from_le_bytes([bytes[2], bytes[3]]));
                if x >= rect.right - 8 && y >= rect.bottom - 8 {
                    return LRESULT(isize::try_from(HTBOTTOMRIGHT).unwrap_or_default());
                }
            }
            _ => {}
        }
    }
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

struct CaptureDc {
    screen: HDC,
    memory: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
}
impl Drop for CaptureDc {
    fn drop(&mut self) {
        unsafe {
            let _ = SelectObject(self.memory, self.old);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.memory);
            ReleaseDC(None, self.screen);
        }
    }
}
fn checked_pixel_bytes(width: i32, height: i32) -> io::Result<usize> {
    usize::try_from(width)
        .ok()
        .zip(usize::try_from(height).ok())
        .and_then(|(w, h)| w.checked_mul(h))
        .filter(|n| *n > 0 && *n <= MAX_PIXELS)
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| io::Error::other("屏幕尺寸超出截图范围 / Screen exceeds capture limit"))
}
fn capture_desktop() -> io::Result<(RECT, Vec<u8>)> {
    let x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    let length = checked_pixel_bytes(width, height)?;
    let screen = unsafe { GetDC(None) };
    if screen.is_invalid() {
        return Err(io::Error::last_os_error());
    }
    let memory = unsafe { CreateCompatibleDC(Some(screen)) };
    if memory.is_invalid() {
        unsafe {
            ReleaseDC(None, screen);
        }
        return Err(io::Error::last_os_error());
    }
    let bitmap = unsafe { CreateCompatibleBitmap(screen, width, height) };
    if bitmap.is_invalid() {
        unsafe {
            let _ = DeleteDC(memory);
            ReleaseDC(None, screen);
        }
        return Err(io::Error::last_os_error());
    }
    let old = unsafe { SelectObject(memory, bitmap.into()) };
    let dc = CaptureDc {
        screen,
        memory,
        bitmap,
        old,
    };
    unsafe {
        BitBlt(
            memory,
            0,
            0,
            width,
            height,
            Some(screen),
            x,
            y,
            SRCCOPY | CAPTUREBLT,
        )
    }
    .map_err(win)?;
    // GetDIBits requires the bitmap to be unselected from the memory DC.
    let _ = unsafe { SelectObject(memory, old) };
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: u32::try_from(std::mem::size_of::<BITMAPINFOHEADER>()).unwrap_or(40),
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut pixels = vec![0; length];
    if unsafe {
        GetDIBits(
            memory,
            bitmap,
            0,
            u32::try_from(height).map_err(io::Error::other)?,
            Some(pixels.as_mut_ptr().cast()),
            &raw mut info,
            DIB_RGB_COLORS,
        )
    } != height
    {
        return Err(io::Error::last_os_error());
    }
    for pixel in pixels.chunks_exact_mut(4) {
        pixel[3] = 255;
    }
    drop(dc);
    Ok((
        RECT {
            left: x,
            top: y,
            right: x + width,
            bottom: y + height,
        },
        pixels,
    ))
}

fn encode_png(width: u32, height: u32, pixels: &[u8]) -> io::Result<Vec<u8>> {
    let factory: IWICImagingFactory =
        unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) }
            .map_err(win)?;
    let stream =
        unsafe { CreateStreamOnHGlobal(windows::Win32::Foundation::HGLOBAL::default(), true) }
            .map_err(win)?;
    let encoder = unsafe { factory.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null()) }
        .map_err(win)?;
    unsafe { encoder.Initialize(&stream, WICBitmapEncoderNoCache) }.map_err(win)?;
    let mut frame = None;
    unsafe { encoder.CreateNewFrame(&raw mut frame, std::ptr::null_mut()) }.map_err(win)?;
    let frame = frame.ok_or_else(|| io::Error::other("Missing PNG encoder"))?;
    unsafe {
        frame.Initialize(None).map_err(win)?;
        frame.SetSize(width, height).map_err(win)?;
        let mut format = GUID_WICPixelFormat32bppBGRA;
        frame.SetPixelFormat(&raw mut format).map_err(win)?;
        if format != GUID_WICPixelFormat32bppBGRA {
            return Err(io::Error::other("Unsupported PNG pixel format"));
        }
        frame.WritePixels(height, width * 4, pixels).map_err(win)?;
        frame.Commit().map_err(win)?;
        encoder.Commit().map_err(win)?;
    }
    let mut stat = STATSTG::default();
    unsafe { stream.Stat(&raw mut stat, STATFLAG_NONAME) }.map_err(win)?;
    let size = usize::try_from(stat.cbSize).map_err(io::Error::other)?;
    if size > MAX_PNG_BYTES {
        return Err(io::Error::other("PNG exceeds clipboard limit"));
    }
    let memory = unsafe { GetHGlobalFromStream(&stream) }.map_err(win)?;
    let pointer = unsafe { GlobalLock(memory) };
    if pointer.is_null() {
        return Err(io::Error::last_os_error());
    }
    let png = unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), size) }.to_vec();
    let _ = unsafe { GlobalUnlock(memory) };
    Ok(png)
}

fn clipboard_memory(bytes: &[u8]) -> io::Result<windows::Win32::Foundation::HGLOBAL> {
    let memory = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len()) }.map_err(win)?;
    let pointer = unsafe { GlobalLock(memory) };
    if pointer.is_null() {
        let _ = unsafe { GlobalFree(Some(memory)) };
        return Err(io::Error::last_os_error());
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.cast(), bytes.len());
        let _ = GlobalUnlock(memory);
    }
    Ok(memory)
}
fn write_clipboard(window: HWND, png: &[u8], dib: &[u8]) -> io::Result<()> {
    // Allocate both formats before replacing the existing clipboard. Windows
    // owns each allocation only after the corresponding SetClipboardData succeeds.
    let png_memory = clipboard_memory(png)?;
    let dib_memory = match clipboard_memory(dib) {
        Ok(memory) => memory,
        Err(error) => {
            let _ = unsafe { GlobalFree(Some(png_memory)) };
            return Err(error);
        }
    };
    let png_format = unsafe { RegisterClipboardFormatW(w!("PNG")) };
    let opened = (0..8).any(|_| {
        if unsafe { OpenClipboard(Some(window)) }.is_ok() {
            true
        } else {
            std::thread::sleep(std::time::Duration::from_millis(15));
            false
        }
    });
    if !opened || png_format == 0 {
        unsafe {
            let _ = GlobalFree(Some(png_memory));
            let _ = GlobalFree(Some(dib_memory));
            if opened {
                let _ = CloseClipboard();
            }
        }
        return Err(io::Error::other(
            "剪贴板忙，请重试 / Clipboard is busy; retry",
        ));
    }
    let mut png_owned = false;
    let mut dib_owned = false;
    let result = (|| unsafe {
        EmptyClipboard().map_err(win)?;
        SetClipboardData(png_format, Some(HANDLE(png_memory.0))).map_err(win)?;
        png_owned = true;
        SetClipboardData(u32::from(CF_DIBV5.0), Some(HANDLE(dib_memory.0))).map_err(win)?;
        dib_owned = true;
        Ok(())
    })();
    unsafe {
        let _ = CloseClipboard();
        if !png_owned {
            let _ = GlobalFree(Some(png_memory));
        }
        if !dib_owned {
            let _ = GlobalFree(Some(dib_memory));
        }
    }
    result
}

fn save_png(window: HWND, png: &[u8]) -> io::Result<bool> {
    let dialog: IFileSaveDialog =
        unsafe { CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER) }.map_err(win)?;
    unsafe {
        dialog
            .SetFileTypes(&[COMDLG_FILTERSPEC {
                pszName: w!("PNG image"),
                pszSpec: w!("*.png"),
            }])
            .map_err(win)?;
        dialog.SetDefaultExtension(w!("png")).map_err(win)?;
        dialog
            .SetFileName(w!("Pierdesk Screenshot.png"))
            .map_err(win)?;
        if let Err(error) = dialog.Show(Some(window)) {
            if error.code() == windows::core::HRESULT(0x8007_04c7_u32.cast_signed()) {
                return Ok(false);
            }
            return Err(win(error));
        }
        let item = dialog.GetResult().map_err(win)?;
        let path = item.GetDisplayName(SIGDN_FILESYSPATH).map_err(win)?;
        let path = CoTaskMemPWSTR::from(path).to_string();
        std::fs::write(path, png)?;
    }
    Ok(true)
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
fn win(error: windows_core::Error) -> io::Error {
    io::Error::other(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wic_roundtrip_preserves_pixels_and_clipboard_compatibility() {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
            .ok()
            .unwrap();
        let _com = Apartment;
        let pixels = vec![10, 20, 30, 255, 200, 100, 0, 255];
        let png = encode_png(2, 1, &pixels).unwrap();
        let dib = crate::clipboard::png_to_dibv5(&png).unwrap();
        assert_eq!(&dib[124..], pixels);
        assert_eq!(
            decode_image(&format!("data:image/png;base64,{}", STANDARD.encode(&png))).unwrap(),
            png
        );
        assert!(decode_image("data:image/jpeg;base64,AAAA").is_err());
        assert!(decode_image("data:image/png;base64,AAAA").is_err());
        assert!(checked_pixel_bytes(-1, 100).is_err());
        assert!(checked_pixel_bytes(100_000, 100_000).is_err());
    }

    #[test]
    fn native_webview_initializes_and_cancels_without_desktop_or_clipboard_input() {
        check_native_editor(false);
    }

    #[test]
    fn prewarmed_editor_stays_idle_and_captures_only_on_activation() {
        check_native_editor(true);
    }

    #[allow(clippy::too_many_lines)] // Exercise one complete hidden window/COM lifetime.
    fn check_native_editor(warm: bool) {
        use windows::Win32::UI::WindowsAndMessaging::{IsWindowVisible, PM_REMOVE, PeekMessageW};
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
            .ok()
            .unwrap();
        let _com = Apartment;
        let _dpi = crate::cross_screen::ThreadDpiAwarenessGuard::enter_per_monitor().unwrap();
        let root = tempfile::tempdir().unwrap();
        let started = std::time::Instant::now();
        let (capture_bounds, capture) = if warm {
            (
                RECT {
                    left: 0,
                    top: 0,
                    right: 640,
                    bottom: 480,
                },
                None,
            )
        } else {
            let (bounds, image, _) = prepare_capture().unwrap();
            (bounds, Some(image))
        };
        let mut state = Box::new(State {
            pixels: RefCell::new(None),
            targets: RefCell::new(Vec::new()),
            detector: Detector::new(),
            host: RefCell::new(None),
            selection: RefCell::new(None),
            capture: RefCell::new(capture),
            pending: RefCell::new(VecDeque::new()),
            shown: Cell::new(false),
            present: false,
            pinned: Cell::new(false),
            done: Cell::new(false),
            bounds: Cell::new(capture_bounds),
            language: RefCell::new("en".into()),
            started: Cell::new(started),
            ready: Cell::new(false),
            awaiting_activation: Cell::new(warm),
            latency_log: None,
            parent: None,
        });
        let instance = HINSTANCE(unsafe { GetModuleHandleW(None) }.unwrap().0);
        let class = WNDCLASSW {
            lpfnWndProc: Some(procedure),
            hInstance: instance,
            lpszClassName: CLASS,
            ..Default::default()
        };
        assert_ne!(unsafe { RegisterClassW(&raw const class) }, 0);
        let window = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW,
                CLASS,
                w!("Pierdesk hidden screenshot test"),
                WS_POPUP,
                0,
                0,
                640,
                480,
                None,
                None,
                Some(instance),
                Some((&raw mut *state).cast()),
            )
        }
        .unwrap();
        let host = create_webview(window, root.path()).unwrap();
        let html = wide(&HTML.replace("/*__EDITOR__*/", SCRIPT));
        unsafe { host.webview.NavigateToString(PCWSTR(html.as_ptr())) }.unwrap();
        *state.host.borrow_mut() = Some(host);
        let pump_until = |predicate: &dyn Fn() -> bool| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            while !predicate() {
                let mut msg = MSG::default();
                while unsafe { PeekMessageW(&raw mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                    unsafe {
                        let _ = TranslateMessage(&raw const msg);
                        DispatchMessageW(&raw const msg);
                    }
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "hidden screenshot startup timed out"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        };
        pump_until(&|| state.ready.get());
        let preparation = state.started.get().elapsed();
        if warm {
            assert!(state.capture.borrow().is_none());
            assert!(state.selection.borrow().is_none());
            assert!(!state.shown.get());
            assert!(!super::super::selection_active());
            state.activate(window, true).unwrap();
        }
        pump_until(&|| state.shown.get());
        eprintln!(
            "SCREENSHOT_TIMING prewarmed={warm} webview_prepare_ms={} capture_to_ready_ms={}",
            preparation.as_millis(),
            state.started.get().elapsed().as_millis()
        );
        assert!(!unsafe { IsWindowVisible(window) }.as_bool());
        let webview = state.host.borrow().as_ref().unwrap().webview.clone();
        let script = wide(
            "JSON.stringify({tools:document.querySelectorAll('[data-tool]').length,width:document.querySelector('canvas').width})",
        );
        let (tx, rx) = mpsc::channel();
        webview2_com::ExecuteScriptCompletedHandler::wait_for_async_operation(
            Box::new(move |handler| {
                unsafe { webview.ExecuteScript(PCWSTR(script.as_ptr()), &handler) }
                    .map_err(webview2_com::Error::WindowsError)
            }),
            Box::new(move |result, value| {
                result?;
                tx.send(value).unwrap();
                Ok(())
            }),
        )
        .unwrap();
        let result: String = serde_json::from_str(&rx.recv().unwrap()).unwrap();
        let result: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(result["tools"], 14);
        assert_eq!(
            result["width"],
            state.bounds.get().right - state.bounds.get().left
        );
        state
            .pending
            .borrow_mut()
            .push_back(r#"{"action":"cancel"}"#.into());
        unsafe { PostMessageW(Some(window), DISPATCH, WPARAM(0), LPARAM(0)) }.unwrap();
        pump_until(&|| state.done.get());
        state.host.borrow_mut().take();
        unsafe {
            DestroyWindow(window).unwrap();
            UnregisterClassW(CLASS, Some(instance)).unwrap();
        }
    }

    #[test]
    fn desktop_capture_reads_physical_pixels_without_saving_or_changing_clipboard() {
        let _dpi = crate::cross_screen::ThreadDpiAwarenessGuard::enter_per_monitor().unwrap();
        let (bounds, pixels) = capture_desktop().unwrap();
        assert_eq!(
            pixels.len(),
            checked_pixel_bytes(bounds.right - bounds.left, bounds.bottom - bounds.top).unwrap()
        );
        assert!(pixels.chunks_exact(4).all(|pixel| pixel[3] == 255));
    }
}
