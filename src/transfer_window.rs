use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::c_void;
use std::fs::{self, OpenOptions};
use std::io;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use webview2_com::{
    CoTaskMemPWSTR, CreateCoreWebView2ControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler,
    Microsoft::Web::WebView2::Win32::{
        COREWEBVIEW2_BOUNDS_MODE_USE_RAW_PIXELS, CreateCoreWebView2EnvironmentWithOptions,
        ICoreWebView2, ICoreWebView2Controller, ICoreWebView2Controller3,
        ICoreWebView2EnvironmentOptions, ICoreWebView2Settings3,
    },
    WebMessageReceivedEventHandler,
};
use windows::Win32::Foundation::{
    COLORREF, E_POINTER, FILETIME, GlobalFree, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT,
    RECT, SIZE, SYSTEMTIME, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, CreateFontW, CreatePen, CreateSolidBrush,
    DEFAULT_CHARSET, DEFAULT_PITCH, DT_END_ELLIPSIS, DT_LEFT, DT_SINGLELINE, DT_VCENTER,
    DeleteObject, DrawFocusRect, DrawTextW, Ellipse, EndPaint, FF_DONTCARE, FillRect,
    GetStockObject, GetTextExtentPoint32W, HDC, HFONT, HGDIOBJ, HOLLOW_BRUSH, IntersectClipRect,
    InvalidateRect, LineTo, MoveToEx, OUT_DEFAULT_PRECIS, PAINTSTRUCT, PS_SOLID, Polygon,
    RestoreDC, RoundRect, SaveDC, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::Storage::FileSystem::FileTimeToLocalFileTime;
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock, GlobalUnlock,
};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::System::Time::FileTimeToSystemTime;
use windows::Win32::UI::Controls::{
    DRAWITEMSTRUCT, ICC_WIN95_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx, ODS_DISABLED,
    ODS_FOCUS, ODS_HOTLIGHT, ODS_SELECTED, TOOLTIP_FLAGS, TOOLTIPS_CLASS, TTF_IDISHWND,
    TTF_SUBCLASS, TTM_ADDTOOLW, TTTOOLINFOW, WM_MOUSELEAVE,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, IsWindowEnabled, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    BS_OWNERDRAW, CREATESTRUCTW, CS_DBLCLKS, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW,
    DestroyWindow, GWL_STYLE, GWLP_USERDATA, GetClientRect, GetSystemMetrics, GetWindowLongPtrW,
    GetWindowPlacement, GetWindowTextW, HICON, HMENU, IDC_ARROW, IDYES, IsDialogMessageW,
    LoadCursorW, MB_ICONQUESTION, MB_SETFOREGROUND, MB_YESNO, MINMAXINFO, MSG, MessageBoxW,
    PostMessageW, RegisterClassW, SIZE_MAXIMIZED, SIZE_MINIMIZED, SIZE_RESTORED, SM_CXSCREEN,
    SM_CYSCREEN, SW_HIDE, SW_RESTORE, SW_SHOWMAXIMIZED, SW_SHOWNOACTIVATE, SendMessageW,
    SetForegroundWindow, SetWindowLongPtrW, SetWindowTextW, ShowWindow, UnregisterClassW,
    WINDOW_EX_STYLE, WINDOW_STYLE, WINDOWPLACEMENT, WM_CANCELMODE, WM_CAPTURECHANGED, WM_CLOSE,
    WM_COMMAND, WM_DRAWITEM, WM_ERASEBKGND, WM_EXITSIZEMOVE, WM_GETMINMAXINFO, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WM_SETFONT, WM_SETREDRAW,
    WM_SIZE, WNDCLASSW, WS_BORDER, WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_EX_APPWINDOW,
    WS_EX_TOPMOST, WS_OVERLAPPEDWINDOW, WS_POPUP, WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
};
use windows_core::{Interface as _, PCWSTR, PWSTR, w};

#[cfg(test)]
use windows::Win32::UI::WindowsAndMessaging::{
    HWND_TOP, IsWindowVisible, SWP_NOACTIVATE, SetWindowPos,
};

use crate::app_settings::{UiLanguage, UiTheme};
use crate::clipboard::{
    ProductCommand, ProductFileSnapshot, ProductSnapshot, ProductTransferDirection,
    ProductTransferId, ProductTransferState, ProductTransferTaskSnapshot,
};

const CLASS: PCWSTR = w!("PierdeskTransferWindow");
const WEB_CONTENT: &str = include_str!("../assets/transfer_window/index.html");
const ROWS: usize = 7;
const WINDOW_WIDTH: i32 = 1360;
const ROW_BASE: usize = 100;
const HEADING: usize = 1;
const SUMMARY: usize = 2;
const PAGES: usize = 3;
const HINT: usize = 4;
const SOURCE_LOCATION: usize = 5;
const TARGET_LOCATION: usize = 6;
const TIME_INFO: usize = 7;
const FILE_LIST: usize = 10;
const FILE_CANCEL: usize = 11;
const BACK: usize = 12;
const PREVIOUS: usize = 13;
const NEXT: usize = 14;
const ALL_PAUSE: usize = 20;
const ALL_RESUME: usize = 21;
const ALL_CANCEL: usize = 22;
const HIDE: usize = 23;
const HISTORY: usize = 24;
const CURRENT_VIEW: usize = 25;
const HISTORY_LIMIT: usize = 20;
const HISTORY_LIMIT_MAX: usize = 500;
const HISTORY_FILE_LIMIT: u64 = 64 * 1024 * 1024;
const HISTORY_HEADER_V1: &str = "ClipFerryTransferHistoryV1";
const HISTORY_HEADER_V2: &str = "ClipFerryTransferHistoryV2";
const HISTORY_HEADER: &str = "ClipFerryTransferHistoryV3";
const VIEW_SETTINGS_HEADER: &str = "ClipFerryTransferViewSettingsV1";
const VIEW_SETTINGS_HEADER_V2: &str = "ClipFerryTransferViewSettingsV2";
const VIEW_SETTINGS_HEADER_V3: &str = "ClipFerryTransferViewSettingsV3";
const VIEW_SETTINGS_FILE_LIMIT: u64 = 4 * 1024;
const HISTORY_ROW_HEIGHT_MIN: u16 = 64;
const HISTORY_ROW_HEIGHT_MAX: u16 = 96;
const HISTORY_ROW_HEIGHT_DEFAULT: u16 = 76;
const WINDOW_MIN_WIDTH: i32 = 760;
const WINDOW_MIN_HEIGHT: i32 = 540;
const LB_GETCOUNT: u32 = 0x018b;
const LB_DELETESTRING: u32 = 0x0182;
const LB_INSERTSTRING: u32 = 0x0181;
const LB_SETSEL: u32 = 0x0185;
const LB_GETSEL: u32 = 0x0187;
const LB_GETTOPINDEX: u32 = 0x018e;
const LB_SETTOPINDEX: u32 = 0x0197;
const LB_SETITEMHEIGHT: u32 = 0x01a0;
const LB_GETCARETINDEX: u32 = 0x019f;
const LBN_SELCHANGE: usize = 1;
const LBS_NOTIFY_STYLE: u32 = 0x0001;
const BUTTON_SUBCLASS_ID: usize = 0x4346_4254;
const PATH_SUBCLASS_ID: usize = 0x4346_5054;

const COLOR_CANVAS: COLORREF = COLORREF(0x00fa_f8f7);
const COLOR_SURFACE: COLORREF = COLORREF(0x00ff_ffff);
const COLOR_BORDER: COLORREF = COLORREF(0x00e2_ded9);
const COLOR_TEXT: COLORREF = COLORREF(0x002b_2521);
const COLOR_MUTED: COLORREF = COLORREF(0x0072_6b66);
const COLOR_BLUE: COLORREF = COLORREF(0x00e8_751a);
const COLOR_BLUE_SOFT: COLORREF = COLORREF(0x00fb_eee5);
const COLOR_RED: COLORREF = COLORREF(0x004a_3fe8);
const COLOR_RED_SOFT: COLORREF = COLORREF(0x00ee_eafa);
const COLOR_DISABLED: COLORREF = COLORREF(0x00b7_b1ad);
const COLOR_BLUE_PRESSED: COLORREF = COLORREF(0x00fa_e8d7);
const COLOR_RED_PRESSED: COLORREF = COLORREF(0x00e2_dff8);

#[derive(Clone, Copy)]
pub struct TransferWindowCommands {
    pub pause: usize,
    pub resume: usize,
    pub cancel: usize,
    pub dispatch: usize,
}

struct ProcedureState {
    target: HWND,
    commands: TransferWindowCommands,
    auto_popup: Cell<bool>,
    hidden: Cell<bool>,
    history_mode: Cell<bool>,
    page: Cell<usize>,
    details: Cell<Option<ProductTransferId>>,
    tasks: RefCell<Vec<ProductTransferTaskSnapshot>>,
    web_tasks: RefCell<Vec<ProductTransferTaskSnapshot>>,
    archived: RefCell<HashSet<ProductTransferId>>,
    history: RefCell<VecDeque<ProductTransferTaskSnapshot>>,
    history_path: PathBuf,
    view_settings_path: PathBuf,
    history_row_height: Cell<u16>,
    history_limit: Cell<usize>,
    history_column_widths: Cell<HistoryColumnWidths>,
    window_geometry: Cell<Option<WindowGeometry>>,
    window_maximized: Cell<bool>,
    webview_user_data: PathBuf,
    timings: RefCell<HashMap<ProductTransferId, TaskTiming>>,
    file_ids: RefCell<Vec<[u8; 16]>>,
    file_items: RefCell<Vec<ProductFileSnapshot>>,
    file_selection: RefCell<Vec<bool>>,
    dismissed_files: RefCell<HashMap<ProductTransferId, HashSet<[u8; 16]>>>,
    file_list: Cell<HWND>,
    pending: RefCell<VecDeque<ProductCommand>>,
    hot_button: Cell<HWND>,
    pressed_button: Cell<HWND>,
    path_controls: Cell<[HWND; 2]>,
    path_texts: RefCell<[String; 2]>,
    path_offsets: Cell<[i32; 2]>,
    hovered_path: Cell<HWND>,
    confirm_cancel: fn(&ProcedureState, HWND, usize, usize) -> bool,
    webview: RefCell<Option<WebViewHost>>,
    webview_ready: Cell<bool>,
    model_json: RefCell<String>,
    language: Cell<UiLanguage>,
    theme: Cell<UiTheme>,
    font: HFONT,
    small_font: HFONT,
    title_font: HFONT,
    heading_font: HFONT,
}

#[derive(Clone, Copy, Default)]
struct TaskTiming {
    started_unix_ms: u64,
    completed_unix_ms: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct HistoryColumnWidths {
    name: u16,
    size: u16,
    status: u16,
    direction: u16,
    completed: u16,
    duration: u16,
    action: u16,
}

impl Default for HistoryColumnWidths {
    fn default() -> Self {
        Self {
            name: 420,
            size: 112,
            status: 132,
            direction: 190,
            completed: 180,
            duration: 104,
            action: 126,
        }
    }
}

impl HistoryColumnWidths {
    fn valid(self) -> bool {
        (220..=800).contains(&self.name)
            && (88..=240).contains(&self.size)
            && (100..=240).contains(&self.status)
            && (130..=420).contains(&self.direction)
            && (150..=320).contains(&self.completed)
            && (84..=220).contains(&self.duration)
            && (100..=220).contains(&self.action)
    }

    fn encode(self) -> String {
        format!(
            "{},{},{},{},{},{},{}",
            self.name,
            self.size,
            self.status,
            self.direction,
            self.completed,
            self.duration,
            self.action,
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WindowGeometry {
    left: i32,
    top: i32,
    width: i32,
    height: i32,
    maximized: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TransferViewSettings {
    history_limit: usize,
    history_row_height: u16,
    history_column_widths: HistoryColumnWidths,
    window_geometry: Option<WindowGeometry>,
}

impl Default for TransferViewSettings {
    fn default() -> Self {
        Self {
            history_limit: HISTORY_LIMIT,
            history_row_height: HISTORY_ROW_HEIGHT_DEFAULT,
            history_column_widths: HistoryColumnWidths::default(),
            window_geometry: None,
        }
    }
}

struct TaskRow {
    text: HWND,
    pause: HWND,
    cancel: HWND,
    details: HWND,
}

struct Controls {
    heading: HWND,
    summary: HWND,
    source_location: HWND,
    target_location: HWND,
    time_info: HWND,
    rows: Vec<TaskRow>,
    pages: HWND,
    previous: HWND,
    next: HWND,
    files: HWND,
    back: HWND,
    cancel_files: HWND,
    hint: HWND,
    pause_all: HWND,
    resume_all: HWND,
    cancel_all: HWND,
    current_view: HWND,
    history: HWND,
    tooltip: HWND,
}

pub struct TransferWindow {
    handle: HWND,
    instance: HINSTANCE,
    state: Box<ProcedureState>,
    controls: Controls,
    fonts: [HFONT; 4],
    _tooltip_texts: Vec<Vec<u16>>,
    presented: HashSet<ProductTransferId>,
    file_rows: Vec<String>,
    rendered_detail: Option<ProductTransferId>,
    icon_cache: HashMap<String, WebIcon>,
    icon_failures: HashSet<String>,
    _com: ComApartment,
}

/// Reads the persisted history count and row height without creating the native transfer window.
#[must_use]
pub fn persisted_view_preferences(history_path: &Path) -> (usize, u16) {
    let view_settings_path = if history_path.as_os_str().is_empty() {
        PathBuf::new()
    } else {
        history_path.with_extension("view.v1")
    };
    let settings = load_view_settings(&view_settings_path).unwrap_or_default();
    (settings.history_limit, settings.history_row_height)
}

struct ComApartment;

impl ComApartment {
    fn enter() -> io::Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
            .ok()
            .map_err(win_error)?;
        Ok(Self)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

struct WebViewHost {
    controller: ICoreWebView2Controller,
    webview: ICoreWebView2,
    message_token: i64,
}

impl Drop for WebViewHost {
    fn drop(&mut self) {
        unsafe {
            let _ = self.webview.remove_WebMessageReceived(self.message_token);
            let _ = self.controller.Close();
        }
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WebIcon {
    key: String,
    width: i32,
    height: i32,
    bgra: Vec<u8>,
}

fn web_icon(key: String, bytes: &[u8]) -> Option<WebIcon> {
    if bytes.len() < 8 {
        return None;
    }
    let width = i32::from_le_bytes(bytes[..4].try_into().ok()?);
    let height = i32::from_le_bytes(bytes[4..8].try_into().ok()?);
    let pixel_bytes = usize::try_from(width.checked_mul(height)?.checked_mul(4)?).ok()?;
    if width <= 0 || height <= 0 || bytes.len() != 8 + pixel_bytes {
        return None;
    }
    Some(WebIcon {
        key,
        width,
        height,
        bgra: bytes[8..].to_vec(),
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WebModel {
    #[serde(rename = "type")]
    kind: &'static str,
    language: &'static str,
    theme: &'static str,
    peer_label: String,
    current: Vec<WebTask>,
    history: Vec<WebTask>,
    now_unix_ms: u64,
    history_row_height: u16,
    history_limit: usize,
    history_column_widths: HistoryColumnWidths,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WebTask {
    key: String,
    name: String,
    files_count: usize,
    directories_count: usize,
    total_size: u64,
    total_size_label: String,
    transferred: u64,
    state: &'static str,
    state_label: &'static str,
    state_rank: u8,
    direction_label: &'static str,
    receiving: bool,
    direction_arrow: &'static str,
    direction_detail: String,
    progress: u16,
    progress_label: String,
    bytes_per_second: u64,
    speed_label: String,
    source_location: String,
    target_location: String,
    started_unix_ms: u64,
    completed_unix_ms: u64,
    started_label: String,
    completed_label: String,
    duration_ms: u64,
    duration_label: String,
    action_rank: u8,
    first_is_directory: bool,
    icon: Option<WebIcon>,
    files: Vec<WebFile>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WebFile {
    key: String,
    name: String,
    size: u64,
    size_label: String,
    transferred: u64,
    bytes_per_second: u64,
    speed_label: String,
    state: &'static str,
    state_label: &'static str,
    progress: u16,
    is_directory: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WebCommand {
    action: String,
    #[serde(default)]
    task: Option<String>,
    #[serde(default)]
    files: Vec<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default, rename = "rowHeight")]
    row_height: Option<u16>,
    #[serde(default, rename = "columnWidths")]
    column_widths: Option<HistoryColumnWidths>,
    #[serde(default, rename = "historyLimit")]
    history_limit: Option<usize>,
}

impl TransferWindow {
    #[allow(clippy::too_many_lines)]
    pub fn create(
        target: HWND,
        icon: HICON,
        commands: TransferWindowCommands,
        history_path: PathBuf,
        auto_popup: bool,
    ) -> io::Result<Self> {
        let _dpi_guard = crate::cross_screen::ThreadDpiAwarenessGuard::enter_per_monitor()?;
        let com = ComApartment::enter()?;
        let instance = HINSTANCE(unsafe { GetModuleHandleW(None) }.map_err(win_error)?.0);
        let font = create_font(-16, 400)?;
        let small_font = create_font(-14, 400)?;
        let title_font = create_font(-18, 600)?;
        let heading_font = create_font(-25, 600)?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(procedure),
            hInstance: instance,
            hIcon: icon,
            hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.map_err(win_error)?,
            style: CS_DBLCLKS,
            lpszClassName: CLASS,
            ..Default::default()
        };
        if unsafe { RegisterClassW(&raw const class) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let webview_user_data = history_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .map_or_else(
                || std::env::temp_dir().join("Pierdesk-WebView2-test"),
                |path| path.join("WebView2"),
            );
        let view_settings_path = if history_path.as_os_str().is_empty() {
            PathBuf::new()
        } else {
            history_path.with_extension("view.v1")
        };
        let view_settings = load_view_settings(&view_settings_path).unwrap_or_default();
        let saved_geometry = view_settings
            .window_geometry
            .map(|geometry| clamp_window_geometry(geometry, primary_screen_size()));
        let (window_x, window_y, window_width, window_height) = saved_geometry
            .map_or(default_window_bounds(), |geometry| {
                (geometry.left, geometry.top, geometry.width, geometry.height)
            });
        let start_maximized = saved_geometry.is_some_and(|geometry| geometry.maximized);
        let mut history = load_history(&history_path).unwrap_or_default();
        while history.len() > view_settings.history_limit {
            history.pop_front();
        }
        let mut state = Box::new(ProcedureState {
            target,
            commands,
            auto_popup: Cell::new(auto_popup),
            hidden: Cell::new(true),
            history_mode: Cell::new(false),
            page: Cell::new(0),
            details: Cell::new(None),
            tasks: RefCell::new(Vec::new()),
            web_tasks: RefCell::new(Vec::new()),
            archived: RefCell::new(HashSet::new()),
            history: RefCell::new(history),
            history_path,
            view_settings_path,
            history_row_height: Cell::new(view_settings.history_row_height),
            history_limit: Cell::new(view_settings.history_limit),
            history_column_widths: Cell::new(view_settings.history_column_widths),
            window_geometry: Cell::new(saved_geometry),
            window_maximized: Cell::new(start_maximized),
            webview_user_data,
            timings: RefCell::new(HashMap::new()),
            file_ids: RefCell::new(Vec::new()),
            file_items: RefCell::new(Vec::new()),
            file_selection: RefCell::new(Vec::new()),
            dismissed_files: RefCell::new(HashMap::new()),
            file_list: Cell::new(HWND::default()),
            pending: RefCell::new(VecDeque::new()),
            hot_button: Cell::new(HWND::default()),
            pressed_button: Cell::new(HWND::default()),
            path_controls: Cell::new([HWND::default(); 2]),
            path_texts: RefCell::new([String::new(), String::new()]),
            path_offsets: Cell::new([0, 0]),
            hovered_path: Cell::new(HWND::default()),
            confirm_cancel: confirm_file_cancel,
            webview: RefCell::new(None),
            webview_ready: Cell::new(false),
            model_json: RefCell::new(String::new()),
            language: Cell::new(UiLanguage::ChineseSimplified),
            theme: Cell::new(UiTheme::Light),
            font,
            small_font,
            title_font,
            heading_font,
        });
        let handle = unsafe {
            CreateWindowExW(
                WS_EX_APPWINDOW,
                CLASS,
                w!("Pierdesk · 传输中心"),
                WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
                window_x,
                window_y,
                window_width,
                window_height,
                None,
                None,
                Some(instance),
                Some((&raw mut *state).cast::<c_void>()),
            )
        }
        .map_err(win_error)?;
        state.window_maximized.set(start_maximized);
        let controls = create_controls(handle, instance, font, heading_font)?;
        state.file_list.set(controls.files);
        state
            .path_controls
            .set([controls.source_location, controls.target_location]);
        install_button_subclasses(&controls, &state)?;
        install_path_subclasses(&controls, &state)?;
        let tooltip_texts = install_tooltips(&controls, handle)?;
        hide_legacy_controls(&controls);
        Ok(Self {
            handle,
            instance,
            state,
            controls,
            fonts: [font, small_font, title_font, heading_font],
            _tooltip_texts: tooltip_texts,
            presented: HashSet::new(),
            file_rows: Vec::new(),
            rendered_detail: None,
            icon_cache: HashMap::new(),
            icon_failures: HashSet::new(),
            _com: com,
        })
    }

    pub fn set_auto_popup(&self, enabled: bool) {
        self.state.auto_popup.set(enabled);
    }

    pub fn set_language(&self, language: UiLanguage) {
        if self.state.language.replace(language) == language {
            return;
        }
        let title = if language == UiLanguage::English {
            "Pierdesk · Transfer Center"
        } else {
            "Pierdesk · 传输中心"
        };
        let _ = set_text(self.handle, title);
        let _ = self.state.post_json(
            &serde_json::json!({ "type": "language", "language": language.key() }).to_string(),
        );
    }

    pub fn set_theme(&self, theme: UiTheme) {
        if self.state.theme.replace(theme) == theme {
            return;
        }
        let _ = self
            .state
            .post_json(&serde_json::json!({ "type": "theme", "theme": theme.key() }).to_string());
    }

    #[must_use]
    pub fn view_preferences(&self) -> (usize, u16) {
        (
            self.state.history_limit.get(),
            self.state.history_row_height.get(),
        )
    }

    pub fn set_history_limit(&self, limit: usize) -> io::Result<()> {
        self.state.set_history_limit(Some(limit))
    }

    pub fn set_history_row_height(&self, row_height: u16) -> io::Result<()> {
        if !(HISTORY_ROW_HEIGHT_MIN..=HISTORY_ROW_HEIGHT_MAX).contains(&row_height) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "传输窗口历史行高超出范围",
            ));
        }
        let previous = self.state.history_row_height.replace(row_height);
        if let Err(error) = self.state.save_view_settings() {
            self.state.history_row_height.set(previous);
            return Err(error);
        }
        Ok(())
    }

    pub fn show_history(&self) -> io::Result<()> {
        self.show()?;
        self.state
            .post_json(&serde_json::json!({ "type": "view", "view": "history" }).to_string())
    }

    pub fn take_commands(&self) -> Vec<ProductCommand> {
        self.state.pending.borrow_mut().drain(..).collect()
    }

    fn sync_web_model(
        &mut self,
        snapshot: &ProductSnapshot,
        current: &[ProductTransferTaskSnapshot],
    ) -> io::Result<()> {
        let now = unix_time_ms();
        let current = current
            .iter()
            .map(|task| self.web_task(task, now, &snapshot.active_peer_label))
            .collect::<Vec<_>>();
        let history = self
            .state
            .history
            .borrow()
            .iter()
            .rev()
            .cloned()
            .collect::<Vec<_>>();
        let history = history
            .iter()
            .map(|task| self.web_task(task, now, &snapshot.active_peer_label))
            .collect::<Vec<_>>();
        let model = WebModel {
            kind: "model",
            language: self.state.language.get().key(),
            theme: self.state.theme.get().key(),
            peer_label: snapshot.active_peer_label.clone(),
            current,
            history,
            now_unix_ms: now,
            history_row_height: self.state.history_row_height.get(),
            history_limit: self.state.history_limit.get(),
            history_column_widths: self.state.history_column_widths.get(),
        };
        let json = serde_json::to_string(&model).map_err(io::Error::other)?;
        self.state.model_json.replace(json);
        self.state.post_model()
    }

    fn web_task(
        &mut self,
        task: &ProductTransferTaskSnapshot,
        now: u64,
        peer_label: &str,
    ) -> WebTask {
        let task_progress_tenths = progress_tenths(
            task.transfer.transferred,
            task.transfer.total_size,
            task.transfer.state,
        );
        let duration_ms = if task.started_unix_ms == 0 {
            0
        } else {
            task.completed_unix_ms
                .max(if task.completed_unix_ms == 0 { now } else { 0 })
                .saturating_sub(task.started_unix_ms)
        };
        WebTask {
            key: task_key(task.id),
            name: task_display_name(task),
            files_count: task.manifest.files,
            directories_count: task.manifest.directories,
            total_size: task.transfer.total_size,
            total_size_label: format_bytes(task.transfer.total_size),
            transferred: task.transfer.transferred,
            state: state_code(task.transfer.state),
            state_label: task.transfer.state.label(),
            state_rank: state_rank(task.transfer.state),
            direction_label: task.id.direction().label(),
            receiving: task.id.direction() == ProductTransferDirection::Receiving,
            direction_arrow: task.id.direction().arrow(),
            direction_detail: match task.id.direction() {
                ProductTransferDirection::Sending => format!("本机 → {peer_label}"),
                ProductTransferDirection::Receiving => format!("{peer_label} → 本机"),
            },
            progress: task_progress_tenths / 10,
            progress_label: format!(
                "{}.{}%",
                task_progress_tenths / 10,
                task_progress_tenths % 10
            ),
            bytes_per_second: task.transfer.bytes_per_second,
            speed_label: transfer_speed_label(task.transfer.state, task.transfer.bytes_per_second),
            source_location: task.source_location.clone(),
            target_location: task.target_location.clone(),
            started_unix_ms: task.started_unix_ms,
            completed_unix_ms: task.completed_unix_ms,
            started_label: if task.started_unix_ms == 0 {
                "等待开始".to_owned()
            } else {
                format_local_time(task.started_unix_ms)
            },
            completed_label: if task.completed_unix_ms == 0 {
                "—".to_owned()
            } else {
                format_local_time(task.completed_unix_ms)
            },
            duration_ms,
            duration_label: if task.started_unix_ms == 0 {
                "—".to_owned()
            } else {
                format_duration(duration_ms)
            },
            action_rank: action_rank(task.transfer.state),
            first_is_directory: task.files.first().is_some_and(|file| file.is_directory),
            icon: self.task_icon(task),
            files: task
                .files
                .iter()
                .map(|file| {
                    let progress = progress_tenths(file.transferred, file.size, file.state) / 10;
                    WebFile {
                        key: encode_hex(&file.id),
                        name: file.name.clone(),
                        size: file.size,
                        size_label: format_bytes(file.size),
                        transferred: file.transferred,
                        bytes_per_second: file.bytes_per_second,
                        speed_label: if file.is_directory {
                            "—".to_owned()
                        } else {
                            transfer_speed_label(
                                if task.transfer.state != ProductTransferState::Running
                                    && file.state == ProductTransferState::Running
                                {
                                    task.transfer.state
                                } else {
                                    file.state
                                },
                                file.bytes_per_second,
                            )
                        },
                        state: state_code(file.state),
                        state_label: file.state.label(),
                        progress,
                        is_directory: file.is_directory,
                    }
                })
                .collect(),
        }
    }

    fn task_icon(&mut self, task: &ProductTransferTaskSnapshot) -> Option<WebIcon> {
        let first = task.files.first()?;
        let type_key = if first.is_directory {
            "folder".to_owned()
        } else {
            Path::new(&first.name)
                .extension()
                .and_then(|extension| extension.to_str())
                .map_or_else(|| "file".to_owned(), str::to_lowercase)
        };
        let base = if task.id.direction() == ProductTransferDirection::Receiving {
            &task.target_location
        } else {
            &task.source_location
        };
        let path = Path::new(base).join(&first.name);
        let path_key = path.to_string_lossy().into_owned();
        let actual_key = format!("path:{path_key}");
        if let Some(icon) = self.icon_cache.get(&actual_key) {
            return Some(icon.clone());
        }
        if !self.icon_failures.contains(&path_key) {
            if let Ok(bytes) = crate::clipboard::capture_shell_icon(&path)
                && let Some(icon) = web_icon(actual_key.clone(), &bytes)
            {
                self.icon_cache.insert(actual_key, icon.clone());
                return Some(icon);
            }
            if terminal(task.transfer.state) {
                self.icon_failures.insert(path_key);
            }
        }
        let fallback_key = format!("type:{type_key}");
        if let Some(icon) = self.icon_cache.get(&fallback_key) {
            return Some(icon.clone());
        }
        let bytes = crate::clipboard::capture_shell_type_icon(&path, first.is_directory).ok()?;
        let icon = web_icon(fallback_key.clone(), &bytes)?;
        self.icon_cache.insert(fallback_key, icon.clone());
        Some(icon)
    }

    #[allow(clippy::too_many_lines)]
    pub fn update(&mut self, snapshot: &ProductSnapshot) -> io::Result<()> {
        let mut raw_tasks = snapshot.transfer_tasks.clone();
        let now = unix_time_ms();
        for task in &mut raw_tasks {
            let mut timings = self.state.timings.borrow_mut();
            let timing = timings.entry(task.id).or_default();
            if timing.started_unix_ms == 0 && task.transfer.started_files != 0 {
                timing.started_unix_ms = now;
            }
            if terminal(task.transfer.state) && timing.completed_unix_ms == 0 {
                timing.completed_unix_ms = now;
                if timing.started_unix_ms == 0 {
                    timing.started_unix_ms = now;
                }
            }
            task.started_unix_ms = timing.started_unix_ms;
            task.completed_unix_ms = timing.completed_unix_ms;
            drop(timings);
            fill_task_locations(task, &snapshot.active_peer_label);
        }
        // Visit every task. Iterator::any short circuits and used to reopen the same batch
        // on successive refresh ticks after the user hid the window.
        let mut started = false;
        for task in &raw_tasks {
            let first_presentation = task.transfer.started_files != 0
                && task.id.direction() == ProductTransferDirection::Receiving
                && self.presented.insert(task.id)
                && !self.state.archived.borrow().contains(&task.id);
            if first_presentation {
                started = true;
            }
        }
        let live = raw_tasks.iter().map(|task| task.id).collect::<HashSet<_>>();
        self.presented.retain(|id| live.contains(id));
        self.state
            .dismissed_files
            .borrow_mut()
            .retain(|id, _| live.contains(id));
        if started && self.state.auto_popup.get() {
            self.state.hidden.set(false);
            self.state.history_mode.set(false);
            self.state.details.set(None);
            self.state.page.set(0);
        }
        // Decide whether a newly observed receive should open the window before archiving terminal
        // tasks. A small transfer can finish between two tray refreshes; archiving it first used to
        // suppress the receive-only popup even though completion balloons are intentionally off in
        // auto-popup mode.
        let mut history_changed = false;
        for task in raw_tasks
            .iter()
            .filter(|task| terminal(task.transfer.state) && task.transfer.started_files != 0)
        {
            history_changed |= self.state.remember_history(task);
            if self.state.hidden.get() {
                self.state.archived.borrow_mut().insert(task.id);
            }
        }
        if history_changed {
            self.state.save_history_best_effort();
        }
        let archived = self.state.archived.borrow();
        let current_tasks = raw_tasks
            .into_iter()
            .filter(|task| {
                task.transfer.state != ProductTransferState::Cancelled
                    && !archived.contains(&task.id)
            })
            .collect::<Vec<_>>();
        drop(archived);
        let history_mode = self.state.history_mode.get();
        let tasks = if history_mode {
            self.state
                .history
                .borrow()
                .iter()
                .rev()
                .cloned()
                .collect::<Vec<_>>()
        } else {
            current_tasks
                .iter()
                .filter(|task| task.id.direction() == ProductTransferDirection::Receiving)
                .cloned()
                .collect()
        };
        let visible_ids = tasks.iter().map(|task| task.id).collect::<HashSet<_>>();
        if self
            .state
            .details
            .get()
            .is_some_and(|id| !visible_ids.contains(&id))
        {
            self.state.details.set(None);
        }
        self.state.page.set(
            self.state
                .page
                .get()
                .min(tasks.len().saturating_sub(1) / ROWS),
        );
        let active = tasks
            .iter()
            .filter(|task| !terminal(task.transfer.state))
            .count();
        let title = if self.state.language.get() == UiLanguage::English {
            format!(
                "Pierdesk · Transfer Center · {} task{}",
                current_tasks.len(),
                if current_tasks.len() == 1 { "" } else { "s" }
            )
        } else {
            format!("Pierdesk · 传输中心 · {} 个任务", current_tasks.len())
        };
        set_text(self.handle, &title)?;
        set_text(
            self.controls.heading,
            if self.state.details.get().is_some() {
                "任务详情"
            } else if history_mode {
                "传输历史"
            } else {
                "文件接收"
            },
        )?;
        let summary = if history_mode {
            if tasks.is_empty() {
                "暂无历史记录".to_owned()
            } else {
                format!(
                    "最近 {} 条已结束任务（最多保留 {HISTORY_LIMIT} 条）",
                    tasks.len()
                )
            }
        } else if tasks.is_empty() {
            "暂无当前传输任务".to_owned()
        } else {
            format!(
                "来自 {}  ·  {} 个任务  ·  {} 个未结束",
                snapshot.active_peer_label,
                tasks.len(),
                active,
            )
        };
        set_text(self.controls.summary, &summary)?;
        let details = self
            .state
            .details
            .get()
            .and_then(|id| tasks.iter().find(|task| task.id == id));
        if let Some(task) = details {
            self.render_files(task, history_mode)?;
        } else {
            self.render_tasks(&tasks, history_mode)?;
        }
        for (index, row) in self.controls.rows.iter().enumerate() {
            let show = details.is_none() && self.state.page.get() * ROWS + index < tasks.len();
            visible(row.text, show);
            visible(row.details, show);
            visible(row.pause, show && !history_mode);
            visible(row.cancel, show && !history_mode);
        }
        for control in [
            self.controls.back,
            self.controls.source_location,
            self.controls.target_location,
            self.controls.time_info,
        ] {
            visible(control, details.is_some());
        }
        visible(self.controls.files, details.is_some());
        visible(
            self.controls.cancel_files,
            details.is_some() && !history_mode,
        );
        visible(self.controls.pages, details.is_none());
        visible(
            self.controls.previous,
            details.is_none() && tasks.len() > ROWS,
        );
        visible(self.controls.next, details.is_none() && tasks.len() > ROWS);
        enabled(
            self.controls.pause_all,
            tasks.iter().any(|task| {
                matches!(
                    task.transfer.state,
                    ProductTransferState::Running | ProductTransferState::AwaitingPaste
                )
            }),
        );
        enabled(
            self.controls.resume_all,
            tasks
                .iter()
                .any(|task| task.transfer.state == ProductTransferState::Paused),
        );
        enabled(self.controls.cancel_all, active != 0);
        for control in [
            self.controls.pause_all,
            self.controls.resume_all,
            self.controls.cancel_all,
        ] {
            visible(control, !history_mode);
        }
        *self.state.tasks.borrow_mut() = tasks;
        self.state.web_tasks.borrow_mut().clone_from(&current_tasks);
        hide_legacy_controls(&self.controls);
        self.sync_web_model(snapshot, &current_tasks)?;
        if started && self.state.auto_popup.get() && !self.state.hidden.get() {
            self.show()?;
        }
        Ok(())
    }

    fn render_tasks(
        &self,
        tasks: &[ProductTransferTaskSnapshot],
        history_mode: bool,
    ) -> io::Result<()> {
        let page = self.state.page.get();
        for (index, row) in self.controls.rows.iter().enumerate() {
            let Some(task) = tasks.get(page * ROWS + index) else {
                continue;
            };
            let percent = progress_tenths(
                task.transfer.transferred,
                task.transfer.total_size,
                task.transfer.state,
            );
            set_text(
                row.text,
                &format!(
                    "{}，{} 个文件，{}，{}.{}%，{}",
                    task_display_name(task),
                    task.manifest.files,
                    task.transfer.state.label(),
                    percent / 10,
                    percent % 10,
                    task_time_summary(task),
                ),
            )?;
            set_text(
                row.pause,
                if task.transfer.state == ProductTransferState::Paused {
                    "继续此任务"
                } else {
                    "暂停此任务"
                },
            )?;
            enabled(row.pause, !terminal(task.transfer.state));
            enabled(row.cancel, !terminal(task.transfer.state));
            let _ = unsafe { InvalidateRect(Some(row.text), None, false) };
            for button in [row.pause, row.cancel, row.details] {
                let _ = unsafe { InvalidateRect(Some(button), None, false) };
            }
        }
        let pages = if tasks.is_empty() {
            "暂无任务".to_owned()
        } else if tasks.len() <= ROWS {
            format!("共 {} 条", tasks.len())
        } else {
            format!("第 {} / {} 页", page + 1, tasks.len().div_ceil(ROWS))
        };
        set_text(self.controls.pages, &pages)?;
        enabled(self.controls.previous, page != 0);
        enabled(self.controls.next, (page + 1) * ROWS < tasks.len());
        set_text(
            self.controls.hint,
            if history_mode {
                "历史记录最多保留 20 条；双击任务可查看源位置和目标位置。"
            } else if tasks.is_empty() {
                "收到新任务时会自动显示；也可以查看最近 20 条传输历史。"
            } else {
                "隐藏窗口不会中断传输  ·  双击任务可查看文件与位置"
            },
        )
    }

    fn render_files(
        &mut self,
        task: &ProductTransferTaskSnapshot,
        history_mode: bool,
    ) -> io::Result<()> {
        set_text(
            self.controls.summary,
            &if history_mode {
                format!(
                    "{} · {}",
                    truncate(&task.manifest.primary_name, 64),
                    task.transfer.state.label()
                )
            } else {
                format!(
                    "{} · {}    Ctrl / Shift 可多选，选择目录会包含其全部内容",
                    truncate(&task.manifest.primary_name, 48),
                    task.transfer.state.label()
                )
            },
        )?;
        let changed_task = self.rendered_detail != Some(task.id);
        if changed_task {
            self.state.path_offsets.set([0, 0]);
        }
        *self.state.path_texts.borrow_mut() =
            [task.source_location.clone(), task.target_location.clone()];
        set_text(
            self.controls.source_location,
            &format!("源位置：{}", task.source_location),
        )?;
        set_text(
            self.controls.target_location,
            &format!("目标位置：{}", task.target_location),
        )?;
        set_text(self.controls.time_info, &task_time_summary(task))?;
        let dismissed = self
            .state
            .dismissed_files
            .borrow()
            .get(&task.id)
            .cloned()
            .unwrap_or_default();
        let files = task
            .files
            .iter()
            .filter(|file| {
                history_mode
                    || (file.state != ProductTransferState::Cancelled
                        && !dismissed.contains(&file.id))
            })
            .cloned()
            .collect::<Vec<_>>();
        let ids = files.iter().map(|file| file.id).collect::<Vec<_>>();
        let rows = files.iter().map(file_row).collect::<Vec<_>>();
        let same_task = self.rendered_detail == Some(task.id);
        let same_files = *self.state.file_ids.borrow() == ids;
        update_file_list(
            self.controls.files,
            &self.file_rows,
            &rows,
            same_task && same_files,
        )?;
        self.file_rows = rows;
        self.rendered_detail = Some(task.id);
        *self.state.file_ids.borrow_mut() = ids;
        *self.state.file_selection.borrow_mut() = (0..files.len())
            .map(|index| send(self.controls.files, LB_GETSEL, index, 0) > 0)
            .collect();
        *self.state.file_items.borrow_mut() = files;
        set_text(
            self.controls.hint,
            if history_mode {
                "历史记录保留完整文件清单、绝对位置与传输时间；双击文件行可查看完整名称。"
            } else {
                "目录会连同全部子项一起选择；取消前会要求确认，确认后项目立即从队列移除。"
            },
        )
    }

    pub fn show(&self) -> io::Result<()> {
        self.state.hidden.set(false);
        self.state.ensure_webview(self.handle)?;
        self.state.post_model()?;
        unsafe {
            let _ = ShowWindow(
                self.handle,
                if self.state.window_maximized.get() {
                    SW_SHOWMAXIMIZED
                } else {
                    SW_RESTORE
                },
            );
            let _ = SetForegroundWindow(self.handle);
        }
        self.state.resize_webview(self.handle);
        Ok(())
    }

    pub fn is_dialog_message(&self, message: &MSG) -> bool {
        unsafe { IsDialogMessageW(self.handle, message) }.as_bool()
    }
}

impl Drop for TransferWindow {
    fn drop(&mut self) {
        self.state.close_webview();
        unsafe {
            let _ = DestroyWindow(self.handle);
            for font in self.fonts {
                let _ = DeleteObject(HGDIOBJ(font.0));
            }
            let _ = UnregisterClassW(CLASS, Some(self.instance));
        }
    }
}

impl ProcedureState {
    fn ensure_webview(&self, window: HWND) -> io::Result<()> {
        if self.webview.borrow().is_some() {
            return Ok(());
        }
        self.webview_ready.set(false);
        let host = create_webview(window, &self.webview_user_data)?;
        let webview = host.webview.clone();
        self.webview.replace(Some(host));
        let html = wide(WEB_CONTENT);
        unsafe { webview.NavigateToString(PCWSTR(html.as_ptr())) }.map_err(win_error)
    }

    fn close_webview(&self) {
        self.webview_ready.set(false);
        self.webview.borrow_mut().take();
    }

    fn resize_webview(&self, window: HWND) {
        let Ok(_dpi) = crate::cross_screen::ThreadDpiAwarenessGuard::enter_per_monitor() else {
            return;
        };
        let Ok(bounds) = webview_bounds(window) else {
            return;
        };
        if let Some(host) = self.webview.borrow().as_ref() {
            let _ = unsafe { host.controller.SetBounds(bounds) };
        }
    }

    fn save_view_settings(&self) -> io::Result<()> {
        save_view_settings(
            &self.view_settings_path,
            TransferViewSettings {
                history_limit: self.history_limit.get(),
                history_row_height: self.history_row_height.get(),
                history_column_widths: self.history_column_widths.get(),
                window_geometry: self.window_geometry.get(),
            },
        )
    }

    fn persist_window_geometry(&self, window: HWND) {
        let Ok(_dpi) = crate::cross_screen::ThreadDpiAwarenessGuard::enter_per_monitor() else {
            return;
        };
        let Ok(mut geometry) = capture_window_geometry(window, primary_screen_size()) else {
            return;
        };
        geometry.maximized = self.window_maximized.get();
        self.window_geometry.set(Some(geometry));
        let _ = self.save_view_settings();
    }

    fn post_model(&self) -> io::Result<()> {
        if !self.webview_ready.get() {
            return Ok(());
        }
        let model = self.model_json.borrow();
        if model.is_empty() {
            return Ok(());
        }
        self.post_json(&model)
    }

    fn post_json(&self, json: &str) -> io::Result<()> {
        let Some(webview) = self
            .webview
            .borrow()
            .as_ref()
            .map(|host| host.webview.clone())
        else {
            return Ok(());
        };
        let json = wide(json);
        unsafe { webview.PostWebMessageAsJson(PCWSTR(json.as_ptr())) }.map_err(win_error)
    }

    fn handle_web_message(&self, window: HWND, json: &str) -> io::Result<()> {
        const WEB_MESSAGE_LIMIT: usize = 256 * 1024;
        if json.len() > WEB_MESSAGE_LIMIT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "传输窗口命令过长",
            ));
        }
        let command: WebCommand = serde_json::from_str(json).map_err(io::Error::other)?;
        match command.action.as_str() {
            "ready" => {
                self.webview_ready.set(true);
                self.post_model()?;
            }
            "pause" | "resume" | "cancel" => {
                let key = command.task.as_deref().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "传输窗口缺少任务标识")
                })?;
                let task = self
                    .web_tasks
                    .borrow()
                    .iter()
                    .find(|task| task_key(task.id) == key)
                    .cloned()
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, "传输任务已结束或不存在")
                    })?;
                if !terminal(task.transfer.state) {
                    self.enqueue(match command.action.as_str() {
                        "pause" => ProductCommand::PauseTask(task.id),
                        "resume" => ProductCommand::ResumeTask(task.id),
                        _ => ProductCommand::CancelTask(task.id),
                    });
                }
            }
            "pause-all" => self.post(self.commands.pause),
            "resume-all" => self.post(self.commands.resume),
            "cancel-all" => self.post(self.commands.cancel),
            "cancel-files" => self.cancel_web_files(window, &command)?,
            "set-history-limit" => self.set_history_limit(command.history_limit)?,
            "set-row-height" => {
                let row_height = command.row_height.ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "传输窗口缺少历史行高")
                })?;
                if !(HISTORY_ROW_HEIGHT_MIN..=HISTORY_ROW_HEIGHT_MAX).contains(&row_height) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "传输窗口历史行高超出范围",
                    ));
                }
                self.history_row_height.set(row_height);
                self.save_view_settings()?;
            }
            "set-column-widths" => {
                let widths = command.column_widths.ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "传输窗口缺少历史列宽")
                })?;
                if !widths.valid() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "传输窗口历史列宽超出范围",
                    ));
                }
                self.history_column_widths.set(widths);
                self.save_view_settings()?;
            }
            "copy-path" | "open-path" => {
                let path = command.path.as_deref().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "传输窗口缺少路径")
                })?;
                let allowed = if command.action == "open-path" {
                    self.known_local_path(path)
                } else {
                    self.known_path(path)
                };
                if !allowed {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "传输窗口拒绝了未知路径",
                    ));
                }
                if command.action == "copy-path" {
                    copy_unicode_text(window, path)?;
                } else {
                    std::process::Command::new("explorer.exe")
                        .arg(path)
                        .spawn()
                        .map(|_| ())?;
                }
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "传输窗口命令不受支持",
                ));
            }
        }
        Ok(())
    }

    fn set_history_limit(&self, limit: Option<usize>) -> io::Result<()> {
        let limit = limit
            .filter(|limit| (1..=HISTORY_LIMIT_MAX).contains(limit))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "历史保存条数应为 1–500"))?;
        let previous = self.history_limit.replace(limit);
        if let Err(error) = self.save_view_settings() {
            self.history_limit.set(previous);
            return Err(error);
        }
        let mut history = self.history.borrow_mut();
        while history.len() > limit {
            history.pop_front();
        }
        drop(history);
        self.save_history_best_effort();
        Ok(())
    }

    fn known_path(&self, path: &str) -> bool {
        self.web_tasks
            .borrow()
            .iter()
            .any(|task| task.source_location == path || task.target_location == path)
            || self
                .history
                .borrow()
                .iter()
                .any(|task| task.source_location == path || task.target_location == path)
    }

    fn known_local_path(&self, path: &str) -> bool {
        Path::new(path).is_absolute()
            && self
                .web_tasks
                .borrow()
                .iter()
                .chain(self.history.borrow().iter())
                .any(|task| match task.id.direction() {
                    ProductTransferDirection::Receiving => task.target_location == path,
                    ProductTransferDirection::Sending => task.source_location == path,
                })
    }

    fn cancel_web_files(&self, window: HWND, command: &WebCommand) -> io::Result<()> {
        const FILE_SELECTION_LIMIT: usize = 4096;
        if command.files.is_empty() || command.files.len() > FILE_SELECTION_LIMIT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "选中文件数量无效",
            ));
        }
        let key = command
            .task
            .as_deref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "传输窗口缺少任务标识"))?;
        let task = self
            .web_tasks
            .borrow()
            .iter()
            .find(|task| task_key(task.id) == key)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "传输任务已结束或不存在"))?;
        let requested = command
            .files
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        let selected_directories = task
            .files
            .iter()
            .filter(|file| file.is_directory && requested.contains(encode_hex(&file.id).as_str()))
            .map(|file| format!("{}\\", file.name.trim_end_matches(['\\', '/'])))
            .collect::<Vec<_>>();
        let selected_rows = task
            .files
            .iter()
            .filter(|file| {
                requested.contains(encode_hex(&file.id).as_str())
                    || selected_directories
                        .iter()
                        .any(|prefix| file.name.starts_with(prefix))
            })
            .cloned()
            .collect::<Vec<_>>();
        let selected = selected_rows
            .iter()
            .filter(|file| !file.is_directory && !terminal(file.state))
            .map(|file| file.id)
            .collect::<Vec<_>>();
        if selected.is_empty() {
            return Ok(());
        }
        let directory_count = selected_rows
            .iter()
            .filter(|file| file.is_directory)
            .count();
        if (self.confirm_cancel)(self, window, selected.len(), directory_count) {
            self.dismissed_files
                .borrow_mut()
                .entry(task.id)
                .or_default()
                .extend(selected_rows.iter().map(|file| file.id));
            self.enqueue(ProductCommand::CancelFiles(task.id, selected));
        }
        Ok(())
    }

    fn post(&self, command: usize) {
        let _ = unsafe { PostMessageW(Some(self.target), WM_COMMAND, WPARAM(command), LPARAM(0)) };
    }

    fn enqueue(&self, command: ProductCommand) {
        self.pending.borrow_mut().push_back(command);
        self.post(self.commands.dispatch);
    }

    #[allow(clippy::too_many_lines)]
    fn command(&self, window: HWND, identifier: usize, notification: usize) {
        match identifier {
            HIDE => {
                self.persist_window_geometry(window);
                self.archive_finished_current_tasks();
                self.hidden.set(true);
                self.history_mode.set(false);
                self.details.set(None);
                self.page.set(0);
                self.close_webview();
                let _ = unsafe { ShowWindow(window, SW_HIDE) };
            }
            CURRENT_VIEW | HISTORY => {
                self.history_mode.set(identifier == HISTORY);
                self.details.set(None);
                self.page.set(0);
                self.post(self.commands.dispatch);
            }
            ALL_PAUSE => self.post(self.commands.pause),
            ALL_RESUME => self.post(self.commands.resume),
            ALL_CANCEL => self.post(self.commands.cancel),
            BACK => {
                self.details.set(None);
                self.post(self.commands.dispatch);
            }
            PREVIOUS => {
                self.page.set(self.page.get().saturating_sub(1));
                self.post(self.commands.dispatch);
            }
            NEXT => {
                self.page.set(self.page.get() + 1);
                self.post(self.commands.dispatch);
            }
            FILE_CANCEL => {
                if let Some(task) = self.details.get() {
                    self.select_descendants_of_selected_directories();
                    // Collect owned values before showing the modal confirmation. MessageBoxW
                    // pumps a nested message loop, so retaining RefCell guards here would let a
                    // progress refresh re-enter this state and panic the COM server process.
                    let selected_rows = {
                        let ids = self.file_ids.borrow();
                        let items = self.file_items.borrow();
                        ids.iter()
                            .zip(items.iter())
                            .enumerate()
                            .filter(|(index, _)| {
                                send(self.file_list.get(), LB_GETSEL, *index, 0) > 0
                            })
                            .map(|(_, (id, item))| (*id, item.is_directory, item.state))
                            .collect::<Vec<_>>()
                    };
                    let selected = selected_rows
                        .iter()
                        .filter(|(_, directory, state)| !directory && !terminal(*state))
                        .map(|(id, _, _)| *id)
                        .collect::<Vec<_>>();
                    if !selected.is_empty()
                        && (self.confirm_cancel)(
                            self,
                            window,
                            selected.len(),
                            selected_rows
                                .iter()
                                .filter(|(_, directory, _)| *directory)
                                .count(),
                        )
                    {
                        self.dismissed_files
                            .borrow_mut()
                            .entry(task)
                            .or_default()
                            .extend(selected_rows.iter().map(|(id, _, _)| *id));
                        self.enqueue(ProductCommand::CancelFiles(task, selected));
                    }
                }
            }
            FILE_LIST if notification == LBN_SELCHANGE => self.cascade_directory_selection(),
            id if (ROW_BASE..ROW_BASE + ROWS * 4).contains(&id) => {
                let row = (id - ROW_BASE) / 4;
                let action = (id - ROW_BASE) % 4;
                // Resolve the exact ID now, before posting to the tray. A later selection or
                // refresh cannot redirect a button click to a different transfer.
                let task = self
                    .tasks
                    .borrow()
                    .get(self.page.get() * ROWS + row)
                    .cloned();
                if let Some(task) = task {
                    match action {
                        0 if notification == 1 => {
                            self.details.set(Some(task.id));
                            self.post(self.commands.dispatch);
                        }
                        1 if !terminal(task.transfer.state) => {
                            self.enqueue(if task.transfer.state == ProductTransferState::Paused {
                                ProductCommand::ResumeTask(task.id)
                            } else {
                                ProductCommand::PauseTask(task.id)
                            });
                        }
                        2 if !terminal(task.transfer.state) => {
                            self.enqueue(ProductCommand::CancelTask(task.id));
                        }
                        3 => {
                            self.details.set(Some(task.id));
                            self.post(self.commands.dispatch);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn remember_history(&self, task: &ProductTransferTaskSnapshot) -> bool {
        let task = history_task(task);
        let mut history = self.history.borrow_mut();
        if let Some(existing) = history.iter_mut().find(|existing| existing.id == task.id) {
            if *existing == task {
                return false;
            }
            *existing = task;
            return true;
        }
        history.push_back(task);
        while history.len() > self.history_limit.get() {
            history.pop_front();
        }
        true
    }

    fn archive_finished_current_tasks(&self) {
        if self.history_mode.get() {
            return;
        }
        let finished = self
            .tasks
            .borrow()
            .iter()
            .filter(|task| terminal(task.transfer.state))
            .map(|task| task.id)
            .collect::<Vec<_>>();
        self.archived.borrow_mut().extend(finished);
        self.save_history_best_effort();
    }

    fn save_history_best_effort(&self) {
        let _ = save_history(&self.history_path, &self.history.borrow());
    }

    fn cascade_directory_selection(&self) {
        let list = self.file_list.get();
        let items = self.file_items.borrow();
        let current = list_selection(list, items.len());
        let previous = self.file_selection.borrow();
        let mut changed = items
            .iter()
            .enumerate()
            .filter(|(index, item)| {
                item.is_directory
                    && current.get(*index) != previous.get(*index)
                    && current.get(*index).is_some()
            })
            .map(|(index, _)| (index, current[index]))
            .collect::<Vec<_>>();
        if changed.is_empty() {
            let caret = send(list, LB_GETCARETINDEX, 0, 0);
            if let Ok(caret) = usize::try_from(caret)
                && items.get(caret).is_some_and(|item| item.is_directory)
            {
                changed.push((caret, current[caret]));
            }
        }
        changed.sort_by_key(|(_, selected)| *selected);
        for (index, selected) in changed {
            set_directory_descendants(list, &items, index, selected);
        }
        drop(previous);
        *self.file_selection.borrow_mut() = list_selection(list, items.len());
        let _ = unsafe { InvalidateRect(Some(list), None, false) };
    }

    fn select_descendants_of_selected_directories(&self) {
        let list = self.file_list.get();
        let items = self.file_items.borrow();
        for (index, item) in items.iter().enumerate() {
            if item.is_directory && send(list, LB_GETSEL, index, 0) > 0 {
                set_directory_descendants(list, &items, index, true);
            }
        }
        *self.file_selection.borrow_mut() = list_selection(list, items.len());
        let _ = unsafe { InvalidateRect(Some(list), None, false) };
    }

    fn draw_item(&self, item: &DRAWITEMSTRUCT) {
        let id = item.CtlID as usize;
        match id {
            HEADING => draw_label(
                item.hDC,
                item.rcItem,
                &window_text(item.hwndItem),
                self.heading_font,
                COLOR_TEXT,
            ),
            SOURCE_LOCATION | TARGET_LOCATION => {
                let index = usize::from(id == TARGET_LOCATION);
                draw_path_label(
                    item.hDC,
                    item.rcItem,
                    if index == 0 {
                        "源位置："
                    } else {
                        "目标位置："
                    },
                    &self.path_texts.borrow()[index],
                    self.path_offsets.get()[index],
                    self.font,
                );
            }
            SUMMARY | PAGES | HINT | TIME_INFO => draw_label(
                item.hDC,
                item.rcItem,
                &window_text(item.hwndItem),
                if id == HINT {
                    self.small_font
                } else {
                    self.font
                },
                COLOR_MUTED,
            ),
            FILE_LIST => {
                let items = self.file_items.borrow();
                if let Some(file) = items.get(item.itemID as usize) {
                    draw_file_item(item, file, self.font, self.small_font);
                }
            }
            id if (ROW_BASE..ROW_BASE + ROWS * 4).contains(&id) => {
                let row = (id - ROW_BASE) / 4;
                let action = (id - ROW_BASE) % 4;
                let tasks = self.tasks.borrow();
                let task = tasks.get(self.page.get() * ROWS + row);
                if action == 0 {
                    if let Some(task) = task {
                        draw_task_card(
                            item.hDC,
                            item.rcItem,
                            task,
                            self.font,
                            self.title_font,
                            self.history_mode.get(),
                        );
                    } else {
                        fill(item.hDC, &item.rcItem, COLOR_CANVAS);
                    }
                } else {
                    let icon = match action {
                        1 if task.is_some_and(|task| {
                            task.transfer.state == ProductTransferState::Paused
                        }) =>
                        {
                            Icon::Play
                        }
                        1 => Icon::Pause,
                        2 => Icon::Cancel,
                        _ => Icon::Details,
                    };
                    draw_button(
                        item,
                        icon,
                        true,
                        self.font,
                        self.hot_button.get() == item.hwndItem,
                        self.pressed_button.get() == item.hwndItem,
                    );
                }
            }
            _ => draw_button(
                item,
                button_icon(id),
                false,
                self.font,
                self.hot_button.get() == item.hwndItem,
                self.pressed_button.get() == item.hwndItem
                    || (id == HISTORY && self.history_mode.get())
                    || (id == CURRENT_VIEW && !self.history_mode.get()),
            ),
        }
    }
}

fn webview_bounds(window: HWND) -> io::Result<RECT> {
    let _dpi = crate::cross_screen::ThreadDpiAwarenessGuard::enter_per_monitor()?;
    let mut bounds = RECT::default();
    unsafe { GetClientRect(window, &raw mut bounds) }.map_err(win_error)?;
    // The transfer window is created under a per-monitor-aware thread context, so
    // GetClientRect already returns the raw pixel size expected by WebView2. Scaling it again
    // makes the HTML surface larger than its HWND and clips the right/bottom content at 125% or
    // 150% display scaling.
    Ok(bounds)
}

fn configure_webview_bounds(controller: &ICoreWebView2Controller, window: HWND) -> io::Result<()> {
    let _dpi = crate::cross_screen::ThreadDpiAwarenessGuard::enter_per_monitor()?;
    if let Ok(controller3) = controller.cast::<ICoreWebView2Controller3>() {
        unsafe { controller3.SetBoundsMode(COREWEBVIEW2_BOUNDS_MODE_USE_RAW_PIXELS) }
            .map_err(win_error)?;
    }
    unsafe { controller.SetBounds(webview_bounds(window)?) }.map_err(win_error)
}

fn create_webview(window: HWND, user_data: &Path) -> io::Result<WebViewHost> {
    let _dpi = crate::cross_screen::ThreadDpiAwarenessGuard::enter_per_monitor()?;
    fs::create_dir_all(user_data)?;
    let user_data = wide(&user_data.to_string_lossy());
    let environment = {
        let (sender, receiver) = mpsc::channel();
        CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
            Box::new(move |handler| unsafe {
                CreateCoreWebView2EnvironmentWithOptions(
                    PCWSTR::null(),
                    PCWSTR(user_data.as_ptr()),
                    None::<&ICoreWebView2EnvironmentOptions>,
                    &handler,
                )
                .map_err(webview2_com::Error::WindowsError)
            }),
            Box::new(move |error, environment| {
                error?;
                sender
                    .send(environment.ok_or_else(|| windows_core::Error::from(E_POINTER)))
                    .map_err(|_| windows_core::Error::from(E_POINTER))?;
                Ok(())
            }),
        )
        .map_err(webview_error)?;
        receiver
            .recv()
            .map_err(|_| io::Error::other("WebView2 环境初始化通道已关闭"))?
            .map_err(win_error)?
    };
    let controller = {
        let (sender, receiver) = mpsc::channel();
        CreateCoreWebView2ControllerCompletedHandler::wait_for_async_operation(
            Box::new(move |handler| unsafe {
                environment
                    .CreateCoreWebView2Controller(window, &handler)
                    .map_err(webview2_com::Error::WindowsError)
            }),
            Box::new(move |error, controller| {
                error?;
                sender
                    .send(controller.ok_or_else(|| windows_core::Error::from(E_POINTER)))
                    .map_err(|_| windows_core::Error::from(E_POINTER))?;
                Ok(())
            }),
        )
        .map_err(webview_error)?;
        receiver
            .recv()
            .map_err(|_| io::Error::other("WebView2 控制器初始化通道已关闭"))?
            .map_err(win_error)?
    };
    configure_webview_bounds(&controller, window)?;
    unsafe { controller.SetIsVisible(true) }.map_err(win_error)?;
    let webview = unsafe { controller.CoreWebView2() }.map_err(win_error)?;
    let settings = unsafe { webview.Settings() }.map_err(win_error)?;
    unsafe {
        settings
            .SetAreDefaultContextMenusEnabled(false)
            .map_err(win_error)?;
        settings.SetAreDevToolsEnabled(false).map_err(win_error)?;
        settings.SetIsStatusBarEnabled(false).map_err(win_error)?;
        settings.SetIsZoomControlEnabled(false).map_err(win_error)?;
        settings
            .SetAreDefaultScriptDialogsEnabled(false)
            .map_err(win_error)?;
    }
    if let Ok(settings3) = settings.cast::<ICoreWebView2Settings3>() {
        let _ = unsafe { settings3.SetAreBrowserAcceleratorKeysEnabled(false) };
    }
    let mut message_token = 0;
    unsafe {
        webview
            .add_WebMessageReceived(
                &WebMessageReceivedEventHandler::create(Box::new(move |_sender, arguments| {
                    let Some(arguments) = arguments else {
                        return Ok(());
                    };
                    let mut message = PWSTR::null();
                    arguments.WebMessageAsJson(&raw mut message)?;
                    let message = CoTaskMemPWSTR::from(message).to_string();
                    let pointer = GetWindowLongPtrW(window, GWLP_USERDATA) as *const ProcedureState;
                    if !pointer.is_null() {
                        let _ = (&*pointer).handle_web_message(window, &message);
                    }
                    Ok(())
                })),
                &raw mut message_token,
            )
            .map_err(win_error)?;
    }
    Ok(WebViewHost {
        controller,
        webview,
        message_token,
    })
}

unsafe extern "system" fn button_subclass(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _subclass_id: usize,
    reference_data: usize,
) -> LRESULT {
    let pointer = reference_data as *const ProcedureState;
    if pointer.is_null() {
        return unsafe { DefSubclassProc(window, message, wparam, lparam) };
    }
    let state = unsafe { &*pointer };
    match message {
        WM_MOUSEMOVE => {
            if state.hot_button.replace(window) != window {
                let mut tracking = TRACKMOUSEEVENT {
                    cbSize: u32::try_from(std::mem::size_of::<TRACKMOUSEEVENT>())
                        .unwrap_or(u32::MAX),
                    dwFlags: TME_LEAVE,
                    hwndTrack: window,
                    ..Default::default()
                };
                let _ = unsafe { TrackMouseEvent(&raw mut tracking) };
                let _ = unsafe { InvalidateRect(Some(window), None, false) };
            }
        }
        WM_MOUSELEAVE => {
            if state.hot_button.get() == window {
                state.hot_button.set(HWND::default());
                let _ = unsafe { InvalidateRect(Some(window), None, false) };
            }
        }
        WM_LBUTTONDOWN => {
            state.pressed_button.set(window);
            let _ = unsafe { InvalidateRect(Some(window), None, false) };
        }
        WM_LBUTTONUP | WM_CAPTURECHANGED | WM_CANCELMODE => {
            if state.pressed_button.get() == window {
                state.pressed_button.set(HWND::default());
                let _ = unsafe { InvalidateRect(Some(window), None, false) };
            }
        }
        WM_NCDESTROY => {
            let result = unsafe { DefSubclassProc(window, message, wparam, lparam) };
            let _ =
                unsafe { RemoveWindowSubclass(window, Some(button_subclass), BUTTON_SUBCLASS_ID) };
            return result;
        }
        _ => {}
    }
    unsafe { DefSubclassProc(window, message, wparam, lparam) }
}

unsafe extern "system" fn path_subclass(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _subclass_id: usize,
    reference_data: usize,
) -> LRESULT {
    let pointer = reference_data as *const ProcedureState;
    if pointer.is_null() {
        return unsafe { DefSubclassProc(window, message, wparam, lparam) };
    }
    let state = unsafe { &*pointer };
    match message {
        WM_MOUSEMOVE => {
            if state.hovered_path.replace(window) != window {
                let mut tracking = TRACKMOUSEEVENT {
                    cbSize: u32::try_from(std::mem::size_of::<TRACKMOUSEEVENT>())
                        .unwrap_or(u32::MAX),
                    dwFlags: TME_LEAVE,
                    hwndTrack: window,
                    ..Default::default()
                };
                let _ = unsafe { TrackMouseEvent(&raw mut tracking) };
            }
        }
        WM_MOUSELEAVE => {
            if state.hovered_path.get() == window {
                state.hovered_path.set(HWND::default());
            }
        }
        WM_LBUTTONUP => {
            let controls = state.path_controls.get();
            let index = usize::from(window == controls[1]);
            if let Some(path) = state.path_texts.borrow().get(index) {
                let _ = copy_unicode_text(window, path);
            }
        }
        WM_NCDESTROY => {
            let result = unsafe { DefSubclassProc(window, message, wparam, lparam) };
            let _ = unsafe { RemoveWindowSubclass(window, Some(path_subclass), PATH_SUBCLASS_ID) };
            return result;
        }
        _ => {}
    }
    unsafe { DefSubclassProc(window, message, wparam, lparam) }
}

unsafe extern "system" fn procedure(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = lparam.0 as *const CREATESTRUCTW;
        if !create.is_null() {
            unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, (*create).lpCreateParams as isize) };
        }
    }
    let pointer = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *const ProcedureState;
    if !pointer.is_null() {
        let state = unsafe { &*pointer };
        match message {
            WM_COMMAND => {
                state.command(window, wparam.0 & 0xffff, (wparam.0 >> 16) & 0xffff);
                return LRESULT(0);
            }
            WM_CLOSE => {
                state.command(window, HIDE, 0);
                return LRESULT(0);
            }
            WM_SIZE => {
                match u32::try_from(wparam.0).unwrap_or(SIZE_MINIMIZED) {
                    SIZE_MAXIMIZED => state.window_maximized.set(true),
                    SIZE_RESTORED => state.window_maximized.set(false),
                    _ => {}
                }
                state.resize_webview(window);
                return LRESULT(0);
            }
            WM_EXITSIZEMOVE => {
                state.persist_window_geometry(window);
                return LRESULT(0);
            }
            WM_GETMINMAXINFO => {
                let info = lparam.0 as *mut MINMAXINFO;
                if !info.is_null() {
                    unsafe {
                        (*info).ptMinTrackSize.x = WINDOW_MIN_WIDTH;
                        (*info).ptMinTrackSize.y = WINDOW_MIN_HEIGHT;
                    }
                    return LRESULT(0);
                }
            }
            WM_DRAWITEM => {
                let item = lparam.0 as *const DRAWITEMSTRUCT;
                if !item.is_null() {
                    state.draw_item(unsafe { &*item });
                    return LRESULT(1);
                }
            }
            WM_ERASEBKGND => {
                let mut rect = RECT::default();
                let _ = unsafe { GetClientRect(window, &raw mut rect) };
                fill(HDC(wparam.0 as *mut c_void), &rect, COLOR_CANVAS);
                return LRESULT(1);
            }
            WM_PAINT => {
                let mut paint = PAINTSTRUCT::default();
                let dc = unsafe { BeginPaint(window, &raw mut paint) };
                let mut rect = RECT::default();
                let _ = unsafe { GetClientRect(window, &raw mut rect) };
                fill(dc, &rect, COLOR_CANVAS);
                unsafe {
                    let _ = EndPaint(window, &raw const paint);
                }
                return LRESULT(0);
            }
            WM_NCDESTROY => {
                state.persist_window_geometry(window);
                state.close_webview();
                unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, 0) };
            }
            _ => {}
        }
    }
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

#[allow(clippy::too_many_lines)]
fn create_controls(
    parent: HWND,
    instance: HINSTANCE,
    font: HFONT,
    heading_font: HFONT,
) -> io::Result<Controls> {
    let common_controls = INITCOMMONCONTROLSEX {
        dwSize: u32::try_from(std::mem::size_of::<INITCOMMONCONTROLSEX>()).unwrap_or(u32::MAX),
        dwICC: ICC_WIN95_CLASSES,
    };
    if !unsafe { InitCommonControlsEx(&raw const common_controls) }.as_bool() {
        return Err(io::Error::last_os_error());
    }
    let heading = child(
        parent,
        instance,
        "STATIC",
        "文件接收",
        [24, 16, 912, 30],
        HEADING,
        0x000d,
        heading_font,
    )?;
    let summary = child(
        parent,
        instance,
        "STATIC",
        "",
        [24, 54, 912, 28],
        SUMMARY,
        0x000d,
        font,
    )?;
    let source_location = child(
        parent,
        instance,
        "STATIC",
        "",
        [24, 92, 912, 24],
        SOURCE_LOCATION,
        0x0100 | 0x000d,
        font,
    )?;
    let target_location = child(
        parent,
        instance,
        "STATIC",
        "",
        [24, 118, 912, 24],
        TARGET_LOCATION,
        0x0100 | 0x000d,
        font,
    )?;
    let time_info = child(
        parent,
        instance,
        "STATIC",
        "",
        [24, 144, 912, 24],
        TIME_INFO,
        0x000d,
        font,
    )?;
    let mut rows = Vec::new();
    for index in 0..ROWS {
        let top = 100 + i32::try_from(index).unwrap_or(0) * 96;
        let base = ROW_BASE + index * 4;
        rows.push(TaskRow {
            text: child(
                parent,
                instance,
                "STATIC",
                "",
                [24, top, 570, 80],
                base,
                0x0100 | 0x000d,
                font,
            )?,
            pause: child(
                parent,
                instance,
                "BUTTON",
                "暂停此任务",
                [610, top + 12, 106, 34],
                base + 1,
                BS_OWNERDRAW as u32,
                font,
            )?,
            cancel: child(
                parent,
                instance,
                "BUTTON",
                "取消此任务",
                [724, top + 12, 106, 34],
                base + 2,
                BS_OWNERDRAW as u32,
                font,
            )?,
            details: child(
                parent,
                instance,
                "BUTTON",
                "查看文件",
                [838, top + 12, 98, 34],
                base + 3,
                BS_OWNERDRAW as u32,
                font,
            )?,
        });
    }
    let pages = child(
        parent,
        instance,
        "STATIC",
        "",
        [24, 498, 400, 26],
        PAGES,
        0x000d,
        font,
    )?;
    let previous = child(
        parent,
        instance,
        "BUTTON",
        "上一页",
        [724, 490, 98, 32],
        PREVIOUS,
        BS_OWNERDRAW as u32,
        font,
    )?;
    let next = child(
        parent,
        instance,
        "BUTTON",
        "下一页",
        [838, 490, 98, 32],
        NEXT,
        BS_OWNERDRAW as u32,
        font,
    )?;
    // Extended selection keeps Ctrl/Shift multi-select; long paths are ellipsized by owner draw
    // while the complete string remains available to accessibility clients.
    let files = child(
        parent,
        instance,
        "LISTBOX",
        "",
        [24, 94, 912, 386],
        FILE_LIST,
        // LBS_NOTIFY is what makes real mouse selection emit LBN_SELCHANGE. Without it,
        // directory cascade worked only in synthetic tests that sent WM_COMMAND themselves.
        WS_BORDER.0 | WS_VSCROLL.0 | 0x0800 | 0x0100 | 0x0040 | 0x0010 | LBS_NOTIFY_STYLE,
        font,
    )?;
    send(files, LB_SETITEMHEIGHT, 0, 38);
    let back = child(
        parent,
        instance,
        "BUTTON",
        "返回任务",
        [24, 490, 110, 32],
        BACK,
        BS_OWNERDRAW as u32,
        font,
    )?;
    let cancel_files = child(
        parent,
        instance,
        "BUTTON",
        "取消选中文件",
        [778, 490, 158, 32],
        FILE_CANCEL,
        BS_OWNERDRAW as u32,
        font,
    )?;
    let hint = child(
        parent,
        instance,
        "STATIC",
        "",
        [24, 532, 912, 26],
        HINT,
        0x000d,
        font,
    )?;
    let pause_all = child(
        parent,
        instance,
        "BUTTON",
        "全部暂停",
        [442, 574, 112, 36],
        ALL_PAUSE,
        BS_OWNERDRAW as u32,
        font,
    )?;
    let resume_all = child(
        parent,
        instance,
        "BUTTON",
        "全部继续",
        [566, 574, 112, 36],
        ALL_RESUME,
        BS_OWNERDRAW as u32,
        font,
    )?;
    let cancel_all = child(
        parent,
        instance,
        "BUTTON",
        "全部取消",
        [690, 574, 122, 36],
        ALL_CANCEL,
        BS_OWNERDRAW as u32,
        font,
    )?;
    let history = child(
        parent,
        instance,
        "BUTTON",
        "历史记录",
        [824, 22, 132, 34],
        HISTORY,
        BS_OWNERDRAW as u32,
        font,
    )?;
    let current_view = child(
        parent,
        instance,
        "BUTTON",
        "当前任务",
        [680, 22, 132, 34],
        CURRENT_VIEW,
        BS_OWNERDRAW as u32,
        font,
    )?;
    let tooltip = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST,
            TOOLTIPS_CLASS,
            PCWSTR::null(),
            WS_POPUP,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            Some(parent),
            None,
            Some(instance),
            None,
        )
    }
    .map_err(win_error)?;
    Ok(Controls {
        heading,
        summary,
        source_location,
        target_location,
        time_info,
        rows,
        pages,
        previous,
        next,
        files,
        back,
        cancel_files,
        hint,
        pause_all,
        resume_all,
        cancel_all,
        current_view,
        history,
        tooltip,
    })
}

fn hide_legacy_controls(controls: &Controls) {
    for control in [
        controls.heading,
        controls.summary,
        controls.source_location,
        controls.target_location,
        controls.time_info,
        controls.pages,
        controls.previous,
        controls.next,
        controls.files,
        controls.back,
        controls.cancel_files,
        controls.hint,
        controls.pause_all,
        controls.resume_all,
        controls.cancel_all,
        controls.current_view,
        controls.history,
    ] {
        visible(control, false);
    }
    for row in &controls.rows {
        for control in [row.text, row.pause, row.cancel, row.details] {
            visible(control, false);
        }
    }
}

fn install_button_subclasses(controls: &Controls, state: &ProcedureState) -> io::Result<()> {
    let mut buttons = Vec::with_capacity(ROWS * 3 + 9);
    for row in &controls.rows {
        buttons.extend([row.pause, row.cancel, row.details]);
    }
    buttons.extend([
        controls.previous,
        controls.next,
        controls.back,
        controls.cancel_files,
        controls.pause_all,
        controls.resume_all,
        controls.cancel_all,
        controls.current_view,
        controls.history,
    ]);
    let reference_data = std::ptr::from_ref(state) as usize;
    for button in buttons {
        if !unsafe {
            SetWindowSubclass(
                button,
                Some(button_subclass),
                BUTTON_SUBCLASS_ID,
                reference_data,
            )
        }
        .as_bool()
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn install_path_subclasses(controls: &Controls, state: &ProcedureState) -> io::Result<()> {
    let reference_data = std::ptr::from_ref(state) as usize;
    for path in [controls.source_location, controls.target_location] {
        if !unsafe {
            SetWindowSubclass(path, Some(path_subclass), PATH_SUBCLASS_ID, reference_data)
        }
        .as_bool()
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn child(
    parent: HWND,
    instance: HINSTANCE,
    class: &str,
    text: &str,
    bounds: [i32; 4],
    id: usize,
    extra: u32,
    font: HFONT,
) -> io::Result<HWND> {
    let class_w = wide(class);
    let text_w = wide(text);
    let mut style = WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS | WINDOW_STYLE(extra);
    if id != 0 && class != "STATIC" {
        style |= WS_TABSTOP;
    }
    let handle = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR(class_w.as_ptr()),
            PCWSTR(text_w.as_ptr()),
            style,
            bounds[0],
            bounds[1],
            bounds[2],
            bounds[3],
            Some(parent),
            (id != 0).then_some(HMENU(id as *mut c_void)),
            Some(instance),
            None,
        )
    }
    .map_err(win_error)?;
    send(handle, WM_SETFONT, font.0 as usize, 0);
    Ok(handle)
}

fn install_tooltips(controls: &Controls, parent: HWND) -> io::Result<Vec<Vec<u16>>> {
    let mut targets = Vec::new();
    for row in &controls.rows {
        targets.extend([
            (row.pause, "暂停或继续此任务"),
            (row.cancel, "取消此任务"),
            (row.details, "查看任务中的文件"),
        ]);
    }
    targets.extend([
        (controls.source_location, "悬停暂停滚动；点击复制完整源位置"),
        (
            controls.target_location,
            "悬停暂停滚动；点击复制完整目标位置",
        ),
        (controls.previous, "上一页"),
        (controls.next, "下一页"),
        (controls.back, "返回任务列表"),
        (controls.cancel_files, "取消选中的文件"),
        (controls.pause_all, "暂停全部任务"),
        (controls.resume_all, "继续全部任务"),
        (controls.cancel_all, "取消全部任务"),
        (controls.current_view, "查看当前接收任务"),
        (controls.history, "查看最近 20 条传输历史"),
    ]);
    let mut texts = targets
        .iter()
        .map(|(_, text)| wide(text))
        .collect::<Vec<_>>();
    for ((target, _), text) in targets.iter().zip(texts.iter_mut()) {
        let mut info = TTTOOLINFOW {
            // comctl32 v5 rejects the v6-only trailing lpReserved field. The v2 size is
            // accepted by both and contains every field used here.
            cbSize: u32::try_from(
                std::mem::size_of::<TTTOOLINFOW>() - std::mem::size_of::<*mut c_void>(),
            )
            .unwrap_or(u32::MAX),
            uFlags: TOOLTIP_FLAGS(TTF_IDISHWND.0 | TTF_SUBCLASS.0),
            hwnd: parent,
            uId: target.0 as usize,
            lpszText: PWSTR(text.as_mut_ptr()),
            ..Default::default()
        };
        if send(controls.tooltip, TTM_ADDTOOLW, 0, (&raw mut info) as isize) == 0 {
            return Err(io::Error::other("无法创建传输按钮提示"));
        }
    }
    Ok(texts)
}

#[derive(Clone, Copy)]
enum Icon {
    Pause,
    Play,
    Cancel,
    Details,
    Back,
    Forward,
    Current,
    History,
}

fn button_icon(id: usize) -> Icon {
    match id {
        ALL_PAUSE => Icon::Pause,
        ALL_RESUME => Icon::Play,
        ALL_CANCEL | FILE_CANCEL => Icon::Cancel,
        NEXT => Icon::Forward,
        CURRENT_VIEW => Icon::Current,
        HISTORY => Icon::History,
        _ => Icon::Back,
    }
}

fn draw_label(dc: HDC, rect: RECT, text: &str, font: HFONT, color: COLORREF) {
    fill(dc, &rect, COLOR_CANVAS);
    draw_text(
        dc,
        text,
        rect,
        font,
        color,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
    );
}

fn draw_path_label(dc: HDC, rect: RECT, label: &str, path: &str, offset: i32, font: HFONT) {
    fill(dc, &rect, COLOR_CANVAS);
    let old_font = unsafe { SelectObject(dc, HGDIOBJ(font.0)) };
    let label_wide = label.encode_utf16().collect::<Vec<_>>();
    let path_wide = path.encode_utf16().collect::<Vec<_>>();
    let mut label_size = SIZE::default();
    let mut path_size = SIZE::default();
    let _ = unsafe { GetTextExtentPoint32W(dc, &label_wide, &raw mut label_size) };
    let _ = unsafe { GetTextExtentPoint32W(dc, &path_wide, &raw mut path_size) };
    let _ = unsafe { SelectObject(dc, old_font) };
    draw_text(
        dc,
        label,
        RECT {
            right: rect.left + label_size.cx + 8,
            ..rect
        },
        font,
        COLOR_MUTED,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
    );
    let path_rect = RECT {
        left: rect.left + label_size.cx + 8,
        ..rect
    };
    if path_size.cx <= path_rect.right - path_rect.left {
        draw_text(
            dc,
            path,
            path_rect,
            font,
            COLOR_TEXT,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER,
        );
        return;
    }
    let gap = 72;
    let cycle = path_size.cx.saturating_add(gap).max(1);
    let offset = offset.rem_euclid(cycle);
    let saved = unsafe { SaveDC(dc) };
    let _ = unsafe {
        IntersectClipRect(
            dc,
            path_rect.left,
            path_rect.top,
            path_rect.right,
            path_rect.bottom,
        )
    };
    for left in [path_rect.left - offset, path_rect.left - offset + cycle] {
        draw_text(
            dc,
            path,
            RECT {
                left,
                right: left + path_size.cx + 2,
                ..path_rect
            },
            font,
            COLOR_TEXT,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER,
        );
    }
    let _ = unsafe { RestoreDC(dc, saved) };
}

fn draw_task_card(
    dc: HDC,
    rect: RECT,
    task: &ProductTransferTaskSnapshot,
    font: HFONT,
    title_font: HFONT,
    history_mode: bool,
) {
    rounded_surface(dc, rect, COLOR_SURFACE, COLOR_BORDER, 8);
    let marker = unsafe { CreateSolidBrush(state_color(task.transfer.state)) };
    let old_marker = unsafe { SelectObject(dc, HGDIOBJ(marker.0)) };
    let _ = unsafe { Ellipse(dc, 17, 17, 27, 27) };
    let _ = unsafe { SelectObject(dc, old_marker) };
    let _ = unsafe { DeleteObject(HGDIOBJ(marker.0)) };
    let content_right = rect.right - if history_mode { 68 } else { 194 };
    draw_text(
        dc,
        &truncate(&task_display_name(task), 58),
        RECT {
            left: 38,
            top: 4,
            right: content_right,
            bottom: 29,
        },
        title_font,
        COLOR_TEXT,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
    );
    let percent = progress_tenths(
        task.transfer.transferred,
        task.transfer.total_size,
        task.transfer.state,
    );
    let meta = if history_mode {
        format!(
            "{} 个文件  ·  {}  ·  {}  ·  {}",
            task.manifest.files,
            format_bytes(task.transfer.total_size),
            task.transfer.state.label(),
            task_time_summary(task)
        )
    } else {
        format!(
            "{} 个文件  ·  {}  ·  {}.{}%  ·  实时速度 {}",
            task.manifest.files,
            task.transfer.state.label(),
            percent / 10,
            percent % 10,
            transfer_speed_label(task.transfer.state, task.transfer.bytes_per_second)
        )
    };
    draw_text(
        dc,
        &meta,
        RECT {
            left: 38,
            top: 29,
            right: content_right,
            bottom: 54,
        },
        font,
        state_color(task.transfer.state),
        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
    );
    if !history_mode {
        let track = RECT {
            left: 38,
            top: 55,
            right: content_right,
            bottom: 59,
        };
        rounded_surface(dc, track, COLOR_BORDER, COLOR_BORDER, 3);
        let width = i64::from((track.right - track.left).max(0)) * i64::from(percent) / 1000;
        if width > 0 {
            rounded_surface(
                dc,
                RECT {
                    right: track.left + i32::try_from(width).unwrap_or(track.right - track.left),
                    ..track
                },
                COLOR_BLUE,
                COLOR_BLUE,
                3,
            );
        }
    }
}

fn task_display_name(task: &ProductTransferTaskSnapshot) -> String {
    let mut roots = Vec::new();
    for file in &task.files {
        let root = file.name.split('\\').next().unwrap_or(&file.name);
        if !root.is_empty() && !roots.contains(&root) {
            roots.push(root);
        }
    }
    if roots.len() <= 1 {
        return roots.first().map_or_else(
            || task.manifest.primary_name.clone(),
            |name| (*name).to_owned(),
        );
    }
    let shown = roots
        .iter()
        .take(3)
        .map(|name| truncate(name, 18))
        .collect::<Vec<_>>()
        .join("、");
    if roots.len() > 3 {
        format!("{shown} 等 {} 项", roots.len())
    } else {
        shown
    }
}

fn fill_task_locations(task: &mut ProductTransferTaskSnapshot, peer_label: &str) {
    if task.source_location.is_empty() {
        task.source_location = format!("路径未由旧版本提供（来自 {peer_label}）");
    }
    if task.target_location.is_empty() {
        "Windows 未返回目标绝对路径".clone_into(&mut task.target_location);
    }
}

fn history_task(task: &ProductTransferTaskSnapshot) -> ProductTransferTaskSnapshot {
    let mut task = task.clone();
    task.transfer.bytes_per_second = 0;
    task.transfer.average_bytes_per_second = 0;
    task.transfer.current_file_name = None;
    task.transfer.current_file_transferred = 0;
    task.transfer.current_file_size = 0;
    task.transfer.recovery_active = false;
    task
}

fn load_view_settings(path: &Path) -> io::Result<TransferViewSettings> {
    let file = OpenOptions::new().read(true).open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file()
        || metadata.len() == 0
        || metadata.len() > VIEW_SETTINGS_FILE_LIMIT
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "传输窗口视图设置文件大小无效",
        ));
    }
    let mut text = String::new();
    file.take(VIEW_SETTINGS_FILE_LIMIT + 1)
        .read_to_string(&mut text)?;
    let mut lines = text.lines();
    let header = lines.next();
    if !matches!(
        header,
        Some(VIEW_SETTINGS_HEADER | VIEW_SETTINGS_HEADER_V2 | VIEW_SETTINGS_HEADER_V3)
    ) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "传输窗口视图设置版本无效",
        ));
    }
    let history_row_height = lines
        .next()
        .and_then(|line| line.strip_prefix("history_row_height="))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "传输窗口视图设置不完整"))?
        .parse::<u16>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "历史行高不是有效数字"))?;
    if !(HISTORY_ROW_HEIGHT_MIN..=HISTORY_ROW_HEIGHT_MAX).contains(&history_row_height) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "历史行高超出范围",
        ));
    }
    if header == Some(VIEW_SETTINGS_HEADER) {
        if lines.next().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "旧版传输窗口视图设置包含多余内容",
            ));
        }
        return Ok(TransferViewSettings {
            history_row_height,
            ..TransferViewSettings::default()
        });
    }
    let history_column_widths = lines
        .next()
        .and_then(|line| line.strip_prefix("history_column_widths="))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "传输窗口列宽设置缺失"))
        .and_then(parse_history_column_widths)?;
    let window_geometry = lines
        .next()
        .and_then(|line| line.strip_prefix("window_geometry="))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "传输窗口几何设置缺失"))
        .and_then(parse_window_geometry)?;
    let history_limit = if header == Some(VIEW_SETTINGS_HEADER_V3) {
        lines
            .next()
            .and_then(|line| line.strip_prefix("history_limit="))
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|limit| (1..=HISTORY_LIMIT_MAX).contains(limit))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "历史保存条数无效"))?
    } else {
        HISTORY_LIMIT
    };
    if lines.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "传输窗口视图设置包含多余内容",
        ));
    }
    Ok(TransferViewSettings {
        history_limit,
        history_row_height,
        history_column_widths,
        window_geometry,
    })
}

fn save_view_settings(path: &Path, settings: TransferViewSettings) -> io::Result<()> {
    if path.as_os_str().is_empty() {
        return Ok(());
    }
    if !(HISTORY_ROW_HEIGHT_MIN..=HISTORY_ROW_HEIGHT_MAX).contains(&settings.history_row_height)
        || !settings.history_column_widths.valid()
        || !(1..=HISTORY_LIMIT_MAX).contains(&settings.history_limit)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "传输窗口视图设置超出范围",
        ));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let geometry = settings.window_geometry.map_or_else(
        || "none".to_owned(),
        |geometry| {
            format!(
                "{},{},{},{},{}",
                geometry.left,
                geometry.top,
                geometry.width,
                geometry.height,
                u8::from(geometry.maximized),
            )
        },
    );
    let text = format!(
        "{VIEW_SETTINGS_HEADER_V3}\nhistory_row_height={}\nhistory_column_widths={}\nwindow_geometry={geometry}\nhistory_limit={}\n",
        settings.history_row_height,
        settings.history_column_widths.encode(),
        settings.history_limit,
    );
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()
}

fn parse_history_column_widths(value: &str) -> io::Result<HistoryColumnWidths> {
    let values = value
        .split(',')
        .map(|part| {
            part.parse::<u16>()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "历史列宽不是有效数字"))
        })
        .collect::<io::Result<Vec<_>>>()?;
    let [name, size, status, direction, completed, duration, action] = values.as_slice() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "历史列宽数量无效",
        ));
    };
    let widths = HistoryColumnWidths {
        name: *name,
        size: *size,
        status: *status,
        direction: *direction,
        completed: *completed,
        duration: *duration,
        action: *action,
    };
    if !widths.valid() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "历史列宽超出范围",
        ));
    }
    Ok(widths)
}

fn parse_window_geometry(value: &str) -> io::Result<Option<WindowGeometry>> {
    if value == "none" {
        return Ok(None);
    }
    let values = value
        .split(',')
        .map(|part| {
            part.parse::<i32>()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "窗口几何设置不是有效数字"))
        })
        .collect::<io::Result<Vec<_>>>()?;
    let [left, top, width, height, maximized] = values.as_slice() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "窗口几何设置数量无效",
        ));
    };
    if *width <= 0 || *height <= 0 || !matches!(*maximized, 0 | 1) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "窗口几何设置超出范围",
        ));
    }
    Ok(Some(WindowGeometry {
        left: *left,
        top: *top,
        width: *width,
        height: *height,
        maximized: *maximized == 1,
    }))
}

fn primary_screen_size() -> (i32, i32) {
    let width = unsafe { GetSystemMetrics(SM_CXSCREEN) }.max(WINDOW_MIN_WIDTH);
    let height = unsafe { GetSystemMetrics(SM_CYSCREEN) }.max(WINDOW_MIN_HEIGHT);
    (width, height)
}

fn default_window_bounds() -> (i32, i32, i32, i32) {
    let dpi = unsafe { windows::Win32::UI::HiDpi::GetDpiForSystem() }.max(96);
    let scale = |value: i32| i32::try_from(i64::from(value) * i64::from(dpi) / 96).unwrap_or(value);
    let (screen_width, screen_height) = primary_screen_size();
    let width = scale(WINDOW_WIDTH)
        .min(screen_width.saturating_sub(32))
        .max(WINDOW_MIN_WIDTH);
    let height = scale(720)
        .min(screen_height.saturating_sub(80))
        .max(WINDOW_MIN_HEIGHT);
    (CW_USEDEFAULT, CW_USEDEFAULT, width, height)
}

fn clamp_window_geometry(
    geometry: WindowGeometry,
    (screen_width, screen_height): (i32, i32),
) -> WindowGeometry {
    let screen_width = screen_width.max(1);
    let screen_height = screen_height.max(1);
    let min_width = WINDOW_MIN_WIDTH.min(screen_width);
    let min_height = WINDOW_MIN_HEIGHT.min(screen_height);
    let width = geometry.width.clamp(min_width, screen_width);
    let height = geometry.height.clamp(min_height, screen_height);
    WindowGeometry {
        left: geometry.left.clamp(0, screen_width.saturating_sub(width)),
        top: geometry.top.clamp(0, screen_height.saturating_sub(height)),
        width,
        height,
        maximized: geometry.maximized,
    }
}

fn capture_window_geometry(window: HWND, screen_size: (i32, i32)) -> io::Result<WindowGeometry> {
    let mut placement = WINDOWPLACEMENT {
        length: u32::try_from(std::mem::size_of::<WINDOWPLACEMENT>()).unwrap_or(u32::MAX),
        ..Default::default()
    };
    unsafe { GetWindowPlacement(window, &raw mut placement) }.map_err(win_error)?;
    let rect = placement.rcNormalPosition;
    let width = rect.right.saturating_sub(rect.left);
    let height = rect.bottom.saturating_sub(rect.top);
    if width <= 0 || height <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "传输窗口几何无效",
        ));
    }
    Ok(clamp_window_geometry(
        WindowGeometry {
            left: rect.left,
            top: rect.top,
            width,
            height,
            maximized: false,
        },
        screen_size,
    ))
}

fn load_history(path: &Path) -> io::Result<VecDeque<ProductTransferTaskSnapshot>> {
    let file = OpenOptions::new().read(true).open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() || metadata.len() == 0 || metadata.len() > HISTORY_FILE_LIMIT
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "传输历史文件大小无效",
        ));
    }
    let mut text = String::new();
    file.take(HISTORY_FILE_LIMIT + 1)
        .read_to_string(&mut text)?;
    decode_history(&text)
}

fn save_history(path: &Path, history: &VecDeque<ProductTransferTaskSnapshot>) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut text = String::from(HISTORY_HEADER);
    text.push('\n');
    for task in history.iter().rev().take(HISTORY_LIMIT_MAX).rev() {
        text.push_str(&encode_history_task(task));
        text.push('\n');
    }
    if text.len() as u64 > HISTORY_FILE_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "传输历史文件超过大小限制",
        ));
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()
}

fn encode_history_task(task: &ProductTransferTaskSnapshot) -> String {
    [
        encode_hex(&task.id.offer_id()),
        history_direction_code(task.id.direction()).to_string(),
        task.generation.to_string(),
        history_state_code(task.transfer.state).to_string(),
        encode_hex(task.manifest.primary_name.as_bytes()),
        task.manifest.items.to_string(),
        task.manifest.files.to_string(),
        task.manifest.directories.to_string(),
        task.manifest.total_size.to_string(),
        task.transfer.transferred.to_string(),
        encode_hex(task.source_location.as_bytes()),
        encode_hex(task.target_location.as_bytes()),
        task.started_unix_ms.to_string(),
        task.completed_unix_ms.to_string(),
        encode_hex(encode_history_files(&task.files).as_bytes()),
    ]
    .join("\t")
}

fn decode_history(text: &str) -> io::Result<VecDeque<ProductTransferTaskSnapshot>> {
    let mut lines = text.lines();
    let header = lines.next();
    let version = match header {
        Some(HISTORY_HEADER_V1) => 1,
        Some(HISTORY_HEADER_V2) => 2,
        Some(HISTORY_HEADER) => 3,
        _ => 0,
    };
    if version == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "传输历史文件版本无效",
        ));
    }
    let mut history = VecDeque::new();
    for line in lines.filter(|line| !line.is_empty()) {
        let fields = line.split('\t').collect::<Vec<_>>();
        if (version == 1 && fields.len() != 11)
            || (version == 2 && fields.len() != 14)
            || (version == 3 && fields.len() != 15)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "传输历史记录字段无效",
            ));
        }
        let offer = decode_hex(fields[0])?;
        let offer_id: [u8; 16] = offer
            .try_into()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "传输历史任务标识无效"))?;
        let offset = usize::from(version == 3);
        let direction = if version == 3 {
            history_direction_from_code(parse_field(fields[1])?)?
        } else {
            ProductTransferDirection::Receiving
        };
        let state = history_state_from_code(parse_field(fields[2 + offset])?)?;
        let primary_name = decode_history_text(fields[3 + offset])?;
        let source_location = decode_history_text_with_limit(fields[9 + offset], 64 * 1024)?;
        let target_location = decode_history_text_with_limit(fields[10 + offset], 64 * 1024)?;
        let files = parse_field(fields[5 + offset])?;
        let total_size = parse_field(fields[7 + offset])?;
        let history_files = if version >= 2 {
            decode_history_files(&decode_history_text_with_limit(
                fields[13 + offset],
                usize::try_from(HISTORY_FILE_LIMIT).unwrap_or(usize::MAX),
            )?)?
        } else {
            Vec::new()
        };
        history.push_back(ProductTransferTaskSnapshot {
            id: ProductTransferId::new(direction, offer_id),
            generation: parse_field(fields[1 + offset])?,
            manifest: crate::clipboard::ProductManifestSummary {
                direction: direction.label(),
                primary_name,
                items: parse_field(fields[4 + offset])?,
                files,
                directories: parse_field(fields[6 + offset])?,
                total_size,
            },
            transfer: crate::clipboard::ProductTransferSnapshot {
                state,
                transferred: parse_field(fields[8 + offset])?,
                total_size,
                bytes_per_second: 0,
                average_bytes_per_second: 0,
                started_files: files,
                total_files: files,
                current_file_name: None,
                current_file_transferred: 0,
                current_file_size: 0,
                reconnect_attempts: 0,
                recovered_commands: 0,
                recovery_active: false,
            },
            files: history_files,
            source_location,
            target_location,
            started_unix_ms: if version >= 2 {
                parse_field(fields[11 + offset])?
            } else {
                0
            },
            completed_unix_ms: if version >= 2 {
                parse_field(fields[12 + offset])?
            } else {
                0
            },
        });
        while history.len() > HISTORY_LIMIT_MAX {
            history.pop_front();
        }
    }
    Ok(history)
}

const fn history_direction_code(direction: ProductTransferDirection) -> u8 {
    match direction {
        ProductTransferDirection::Sending => 1,
        ProductTransferDirection::Receiving => 2,
    }
}

fn history_direction_from_code(code: u8) -> io::Result<ProductTransferDirection> {
    match code {
        1 => Ok(ProductTransferDirection::Sending),
        2 => Ok(ProductTransferDirection::Receiving),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "传输历史方向无效",
        )),
    }
}

fn parse_field<T>(text: &str) -> io::Result<T>
where
    T: std::str::FromStr,
{
    text.parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "传输历史记录包含无效数字"))
}

const fn history_state_code(state: ProductTransferState) -> u8 {
    match state {
        ProductTransferState::Completed => 1,
        ProductTransferState::Cancelled => 2,
        ProductTransferState::Failed => 3,
        ProductTransferState::AwaitingPaste
        | ProductTransferState::Running
        | ProductTransferState::Paused => 0,
    }
}

fn history_state_from_code(code: u8) -> io::Result<ProductTransferState> {
    match code {
        1 => Ok(ProductTransferState::Completed),
        2 => Ok(ProductTransferState::Cancelled),
        3 => Ok(ProductTransferState::Failed),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "传输历史记录状态无效",
        )),
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn decode_hex(text: &str) -> io::Result<Vec<u8>> {
    decode_hex_with_limit(text, 8 * 1024)
}

fn decode_hex_with_limit(text: &str, limit: usize) -> io::Result<Vec<u8>> {
    if !text.len().is_multiple_of(2) || text.len() / 2 > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "传输历史文本长度无效",
        ));
    }
    text.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = decode_nibble(pair[0])?;
            let low = decode_nibble(pair[1])?;
            Ok((high << 4) | low)
        })
        .collect()
}

fn decode_history_text(text: &str) -> io::Result<String> {
    String::from_utf8(decode_hex(text)?)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "传输历史文本不是有效 UTF-8"))
}

fn decode_history_text_with_limit(text: &str, limit: usize) -> io::Result<String> {
    String::from_utf8(decode_hex_with_limit(text, limit)?)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "传输历史文本不是有效 UTF-8"))
}

fn encode_history_files(files: &[ProductFileSnapshot]) -> String {
    files
        .iter()
        .map(|file| {
            [
                encode_hex(&file.id),
                encode_hex(file.name.as_bytes()),
                file.size.to_string(),
                file.transferred.to_string(),
                file_state_code(file.state).to_string(),
                u8::from(file.is_directory).to_string(),
            ]
            .join(",")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn decode_history_files(text: &str) -> io::Result<Vec<ProductFileSnapshot>> {
    text.lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let fields = line.split(',').collect::<Vec<_>>();
            if fields.len() != 6 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "传输历史文件清单字段无效",
                ));
            }
            let id: [u8; 16] = decode_hex(fields[0])?
                .try_into()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "传输历史文件标识无效"))?;
            Ok(ProductFileSnapshot {
                id,
                name: decode_history_text_with_limit(fields[1], 64 * 1024)?,
                size: parse_field(fields[2])?,
                transferred: parse_field(fields[3])?,
                bytes_per_second: 0,
                state: file_state_from_code(parse_field(fields[4])?)?,
                is_directory: match fields[5] {
                    "0" => false,
                    "1" => true,
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "传输历史文件目录标志无效",
                        ));
                    }
                },
            })
        })
        .collect()
}

const fn file_state_code(state: ProductTransferState) -> u8 {
    match state {
        ProductTransferState::AwaitingPaste => 0,
        ProductTransferState::Running => 1,
        ProductTransferState::Paused => 2,
        ProductTransferState::Completed => 3,
        ProductTransferState::Cancelled => 4,
        ProductTransferState::Failed => 5,
    }
}

fn file_state_from_code(code: u8) -> io::Result<ProductTransferState> {
    match code {
        0 => Ok(ProductTransferState::AwaitingPaste),
        1 => Ok(ProductTransferState::Running),
        2 => Ok(ProductTransferState::Paused),
        3 => Ok(ProductTransferState::Completed),
        4 => Ok(ProductTransferState::Cancelled),
        5 => Ok(ProductTransferState::Failed),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "传输历史文件状态无效",
        )),
    }
}

fn decode_nibble(value: u8) -> io::Result<u8> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "传输历史包含无效十六进制字符",
        )),
    }
}

fn draw_button(
    item: &DRAWITEMSTRUCT,
    icon: Icon,
    icon_only: bool,
    font: HFONT,
    tracked_hot: bool,
    tracked_pressed: bool,
) {
    let disabled = item.itemState.0 & ODS_DISABLED.0 != 0;
    let pressed = tracked_pressed || item.itemState.0 & ODS_SELECTED.0 != 0;
    let hot = tracked_hot || item.itemState.0 & ODS_HOTLIGHT.0 != 0;
    let focus = item.itemState.0 & ODS_FOCUS.0 != 0;
    let danger = matches!(icon, Icon::Cancel);
    let background = if disabled {
        COLOR_CANVAS
    } else if pressed {
        if danger {
            COLOR_RED_PRESSED
        } else {
            COLOR_BLUE_PRESSED
        }
    } else if hot {
        if danger {
            COLOR_RED_SOFT
        } else {
            COLOR_BLUE_SOFT
        }
    } else {
        COLOR_SURFACE
    };
    let border = if !disabled && (pressed || hot) {
        if danger { COLOR_RED } else { COLOR_BLUE }
    } else {
        COLOR_BORDER
    };
    rounded_surface(item.hDC, item.rcItem, background, border, 7);
    let color = if disabled {
        COLOR_DISABLED
    } else if danger {
        COLOR_RED
    } else {
        COLOR_BLUE
    };
    let offset = i32::from(pressed && !disabled);
    if icon_only {
        let mut icon_center = center(&item.rcItem);
        icon_center.x += offset;
        icon_center.y += offset;
        draw_icon(item.hDC, icon, icon_center, color);
    } else {
        let label = window_text(item.hwndItem);
        let icon_center = POINT {
            x: item.rcItem.left + 24 + offset,
            y: i32::midpoint(item.rcItem.top, item.rcItem.bottom) + offset,
        };
        draw_icon(item.hDC, icon, icon_center, color);
        draw_text(
            item.hDC,
            &label,
            RECT {
                left: item.rcItem.left + 42 + offset,
                top: item.rcItem.top + offset,
                bottom: item.rcItem.bottom + offset,
                right: item.rcItem.right - 10,
            },
            font,
            if disabled { COLOR_DISABLED } else { COLOR_TEXT },
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
        );
    }
    if focus {
        let focus_rect = inset(item.rcItem, 4);
        unsafe {
            let _ = DrawFocusRect(item.hDC, &raw const focus_rect);
        }
    }
}

#[allow(clippy::too_many_lines)]
fn draw_icon(dc: HDC, icon: Icon, center: POINT, color: COLORREF) {
    let pen = unsafe { CreatePen(PS_SOLID, 2, color) };
    let brush = unsafe { CreateSolidBrush(color) };
    let old_pen = unsafe { SelectObject(dc, HGDIOBJ(pen.0)) };
    let old_brush = unsafe { SelectObject(dc, HGDIOBJ(brush.0)) };
    match icon {
        Icon::Pause => {
            fill(
                dc,
                &RECT {
                    left: center.x - 6,
                    top: center.y - 8,
                    right: center.x - 2,
                    bottom: center.y + 8,
                },
                color,
            );
            fill(
                dc,
                &RECT {
                    left: center.x + 2,
                    top: center.y - 8,
                    right: center.x + 6,
                    bottom: center.y + 8,
                },
                color,
            );
        }
        Icon::Play => {
            let points = [
                POINT {
                    x: center.x - 5,
                    y: center.y - 9,
                },
                POINT {
                    x: center.x - 5,
                    y: center.y + 9,
                },
                POINT {
                    x: center.x + 9,
                    y: center.y,
                },
            ];
            let _ = unsafe { Polygon(dc, &points) };
        }
        Icon::Cancel => {
            let _ = unsafe { SelectObject(dc, GetStockObject(HOLLOW_BRUSH)) };
            let _ = unsafe { Ellipse(dc, center.x - 9, center.y - 9, center.x + 9, center.y + 9) };
            line(dc, center.x - 4, center.y - 4, center.x + 4, center.y + 4);
            line(dc, center.x + 4, center.y - 4, center.x - 4, center.y + 4);
            let _ = unsafe { SelectObject(dc, HGDIOBJ(brush.0)) };
        }
        Icon::Details => {
            for offset in [-6, 0, 6] {
                let _ = unsafe {
                    Ellipse(
                        dc,
                        center.x - 8,
                        center.y + offset - 1,
                        center.x - 6,
                        center.y + offset + 1,
                    )
                };
                line(
                    dc,
                    center.x - 3,
                    center.y + offset,
                    center.x + 8,
                    center.y + offset,
                );
            }
        }
        Icon::Back => {
            line(dc, center.x + 5, center.y - 7, center.x - 3, center.y);
            line(dc, center.x - 3, center.y, center.x + 5, center.y + 7);
        }
        Icon::Forward => {
            line(dc, center.x - 5, center.y - 7, center.x + 3, center.y);
            line(dc, center.x + 3, center.y, center.x - 5, center.y + 7);
        }
        Icon::Current => {
            let _ = unsafe { SelectObject(dc, GetStockObject(HOLLOW_BRUSH)) };
            let _ = unsafe {
                RoundRect(
                    dc,
                    center.x - 10,
                    center.y - 7,
                    center.x + 10,
                    center.y + 7,
                    3,
                    3,
                )
            };
            let _ = unsafe { SelectObject(dc, HGDIOBJ(brush.0)) };
            let _ = unsafe { Ellipse(dc, center.x - 2, center.y - 2, center.x + 2, center.y + 2) };
        }
        Icon::History => {
            let _ = unsafe { SelectObject(dc, GetStockObject(HOLLOW_BRUSH)) };
            let _ = unsafe { Ellipse(dc, center.x - 9, center.y - 9, center.x + 9, center.y + 9) };
            line(dc, center.x, center.y, center.x, center.y - 5);
            line(dc, center.x, center.y, center.x + 5, center.y + 2);
            let _ = unsafe { SelectObject(dc, HGDIOBJ(brush.0)) };
        }
    }
    let _ = unsafe { SelectObject(dc, old_brush) };
    let _ = unsafe { DeleteObject(HGDIOBJ(brush.0)) };
    let _ = unsafe { SelectObject(dc, old_pen) };
    let _ = unsafe { DeleteObject(HGDIOBJ(pen.0)) };
}

fn draw_file_item(item: &DRAWITEMSTRUCT, file: &ProductFileSnapshot, font: HFONT, small: HFONT) {
    let selected = item.itemState.0 & ODS_SELECTED.0 != 0;
    fill(
        item.hDC,
        &item.rcItem,
        if selected {
            COLOR_BLUE_SOFT
        } else {
            COLOR_SURFACE
        },
    );
    let status = if file.state == ProductTransferState::AwaitingPaste {
        "排队中"
    } else {
        file.state.label()
    };
    let percent = progress_tenths(file.transferred, file.size, file.state);
    let status_text = if file.is_directory {
        "目录".to_owned()
    } else {
        format!("{status}  {}.{}%", percent / 10, percent % 10)
    };
    draw_text(
        item.hDC,
        &status_text,
        RECT {
            left: item.rcItem.left + 12,
            right: item.rcItem.left + 132,
            ..item.rcItem
        },
        small,
        state_color(file.state),
        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
    );
    let depth = file.name.split('\\').count().saturating_sub(1).min(10);
    let tree_left = item.rcItem.left + 138;
    let indent = i32::try_from(depth).unwrap_or(10) * 20;
    let row_center = i32::midpoint(item.rcItem.top, item.rcItem.bottom);
    if depth != 0 {
        let guide_pen = unsafe { CreatePen(PS_SOLID, 1, COLOR_BORDER) };
        let old_pen = unsafe { SelectObject(item.hDC, HGDIOBJ(guide_pen.0)) };
        for level in 0..depth {
            let x = tree_left + i32::try_from(level).unwrap_or(0) * 20 + 7;
            line(item.hDC, x, item.rcItem.top, x, item.rcItem.bottom);
        }
        let branch_x = tree_left + indent - 13;
        line(
            item.hDC,
            branch_x,
            row_center,
            tree_left + indent,
            row_center,
        );
        let _ = unsafe { SelectObject(item.hDC, old_pen) };
        let _ = unsafe { DeleteObject(HGDIOBJ(guide_pen.0)) };
    }
    let icon_left = tree_left + indent;
    draw_tree_item_icon(
        item.hDC,
        icon_left,
        row_center - 8,
        file.is_directory,
        selected,
    );
    let name = file.name.rsplit('\\').next().unwrap_or(&file.name);
    draw_text(
        item.hDC,
        name,
        RECT {
            left: icon_left + 22,
            right: item.rcItem.right - 170,
            ..item.rcItem
        },
        font,
        COLOR_TEXT,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
    );
    if !file.is_directory {
        draw_text(
            item.hDC,
            &format!(
                "{} · {} / {}",
                transfer_speed_label(file.state, file.bytes_per_second),
                format_bytes(file.transferred),
                format_bytes(file.size)
            ),
            RECT {
                left: item.rcItem.right - 164,
                right: item.rcItem.right - 12,
                ..item.rcItem
            },
            small,
            COLOR_MUTED,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
        );
    }
}

fn draw_tree_item_icon(dc: HDC, left: i32, top: i32, directory: bool, selected: bool) {
    let outline = if selected { COLOR_BLUE } else { COLOR_MUTED };
    let pen = unsafe { CreatePen(PS_SOLID, 1, outline) };
    let old_pen = unsafe { SelectObject(dc, HGDIOBJ(pen.0)) };
    let brush = unsafe {
        CreateSolidBrush(if directory {
            COLOR_BLUE_SOFT
        } else {
            COLOR_SURFACE
        })
    };
    let old_brush = unsafe { SelectObject(dc, HGDIOBJ(brush.0)) };
    if directory {
        let points = [
            POINT {
                x: left,
                y: top + 3,
            },
            POINT {
                x: left + 7,
                y: top + 3,
            },
            POINT {
                x: left + 10,
                y: top + 6,
            },
            POINT {
                x: left + 18,
                y: top + 6,
            },
            POINT {
                x: left + 18,
                y: top + 15,
            },
            POINT {
                x: left,
                y: top + 15,
            },
        ];
        let _ = unsafe { Polygon(dc, &points) };
    } else {
        let _ = unsafe { MoveToEx(dc, left + 2, top, None) };
        let _ = unsafe { LineTo(dc, left + 13, top) };
        let _ = unsafe { LineTo(dc, left + 18, top + 5) };
        let _ = unsafe { LineTo(dc, left + 18, top + 16) };
        let _ = unsafe { LineTo(dc, left + 2, top + 16) };
        let _ = unsafe { LineTo(dc, left + 2, top) };
        let _ = unsafe { MoveToEx(dc, left + 13, top, None) };
        let _ = unsafe { LineTo(dc, left + 13, top + 5) };
        let _ = unsafe { LineTo(dc, left + 18, top + 5) };
    }
    let _ = unsafe { SelectObject(dc, old_brush) };
    let _ = unsafe { SelectObject(dc, old_pen) };
    let _ = unsafe { DeleteObject(HGDIOBJ(brush.0)) };
    let _ = unsafe { DeleteObject(HGDIOBJ(pen.0)) };
}

fn state_color(state: ProductTransferState) -> COLORREF {
    match state {
        ProductTransferState::Cancelled | ProductTransferState::Failed => COLOR_RED,
        ProductTransferState::Paused | ProductTransferState::AwaitingPaste => COLOR_MUTED,
        ProductTransferState::Running | ProductTransferState::Completed => COLOR_BLUE,
    }
}

fn rounded_surface(dc: HDC, rect: RECT, background: COLORREF, border: COLORREF, radius: i32) {
    let brush = unsafe { CreateSolidBrush(background) };
    let pen = unsafe { CreatePen(PS_SOLID, 1, border) };
    let old_brush = unsafe { SelectObject(dc, HGDIOBJ(brush.0)) };
    let old_pen = unsafe { SelectObject(dc, HGDIOBJ(pen.0)) };
    let _ = unsafe {
        RoundRect(
            dc,
            rect.left,
            rect.top,
            rect.right,
            rect.bottom,
            radius,
            radius,
        )
    };
    let _ = unsafe { SelectObject(dc, old_brush) };
    let _ = unsafe { SelectObject(dc, old_pen) };
    let _ = unsafe { DeleteObject(HGDIOBJ(brush.0)) };
    let _ = unsafe { DeleteObject(HGDIOBJ(pen.0)) };
}

fn fill(dc: HDC, rect: &RECT, color: COLORREF) {
    let brush = unsafe { CreateSolidBrush(color) };
    unsafe {
        FillRect(dc, rect, brush);
        let _ = DeleteObject(HGDIOBJ(brush.0));
    }
}

fn draw_text(
    dc: HDC,
    text: &str,
    mut rect: RECT,
    font: HFONT,
    color: COLORREF,
    format: windows::Win32::Graphics::Gdi::DRAW_TEXT_FORMAT,
) {
    let old_font = unsafe { SelectObject(dc, HGDIOBJ(font.0)) };
    unsafe {
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, color);
    }
    let mut text = text.encode_utf16().collect::<Vec<_>>();
    unsafe {
        DrawTextW(dc, &mut text, &raw mut rect, format);
        let _ = SelectObject(dc, old_font);
    }
}

fn line(dc: HDC, x1: i32, y1: i32, x2: i32, y2: i32) {
    unsafe {
        let _ = MoveToEx(dc, x1, y1, None);
        let _ = LineTo(dc, x2, y2);
    }
}

fn center(rect: &RECT) -> POINT {
    POINT {
        x: i32::midpoint(rect.left, rect.right),
        y: i32::midpoint(rect.top, rect.bottom),
    }
}

fn inset(rect: RECT, amount: i32) -> RECT {
    RECT {
        left: rect.left + amount,
        top: rect.top + amount,
        right: rect.right - amount,
        bottom: rect.bottom - amount,
    }
}

fn window_text(window: HWND) -> String {
    let mut text = vec![0_u16; 256];
    let length = usize::try_from(unsafe { GetWindowTextW(window, &mut text) }).unwrap_or(0);
    String::from_utf16_lossy(&text[..length])
}

fn list_selection(list: HWND, count: usize) -> Vec<bool> {
    (0..count)
        .map(|index| send(list, LB_GETSEL, index, 0) > 0)
        .collect()
}

fn set_directory_descendants(
    list: HWND,
    items: &[ProductFileSnapshot],
    directory_index: usize,
    selected: bool,
) {
    let Some(directory) = items.get(directory_index).filter(|item| item.is_directory) else {
        return;
    };
    let prefix = format!("{}\\", directory.name.trim_end_matches('\\'));
    for (index, item) in items.iter().enumerate() {
        if index != directory_index && item.name.starts_with(&prefix) {
            send(
                list,
                LB_SETSEL,
                usize::from(selected),
                isize::try_from(index).unwrap_or(isize::MAX),
            );
        }
    }
}

fn confirm_file_cancel(
    _state: &ProcedureState,
    window: HWND,
    files: usize,
    directories: usize,
) -> bool {
    if cfg!(test) {
        return true;
    }
    let scope = if directories == 0 {
        format!("确定取消选中的 {files} 个未完成文件？")
    } else {
        format!(
            "确定取消选中的 {files} 个未完成文件？\r\n\r\n其中包含 {directories} 个目录；目录下的全部未完成文件都会一起取消。"
        )
    };
    let message = wide(&format!(
        "{scope}\r\n\r\n确认后，这些项目会立即从任务队列中移除，且不能在本次任务中继续。"
    ));
    let title = wide("Pierdesk · 确认取消所选文件");
    unsafe {
        MessageBoxW(
            Some(window),
            PCWSTR(message.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_YESNO | MB_ICONQUESTION | MB_SETFOREGROUND,
        ) == IDYES
    }
}

fn update_file_list(
    list: HWND,
    previous: &[String],
    rows: &[String],
    preserve: bool,
) -> io::Result<()> {
    if preserve && previous == rows {
        return Ok(());
    }
    let selected = (0..previous.len())
        .filter(|index| preserve && send(list, LB_GETSEL, *index, 0) > 0)
        .collect::<Vec<_>>();
    let top = send(list, LB_GETTOPINDEX, 0, 0);
    send(list, WM_SETREDRAW, 0, 0);
    let result = (|| {
        let count = usize::try_from(send(list, LB_GETCOUNT, 0, 0)).unwrap_or(0);
        for index in (rows.len()..count).rev() {
            send(list, LB_DELETESTRING, index, 0);
        }
        for (index, row) in rows.iter().enumerate() {
            if preserve && previous.get(index) == Some(row) {
                continue;
            }
            if index < count.min(rows.len()) {
                send(list, LB_DELETESTRING, index, 0);
            }
            let row = wide(row);
            if send(list, LB_INSERTSTRING, index, row.as_ptr() as isize) < 0 {
                return Err(io::Error::other("无法更新文件列表"));
            }
        }
        send(list, LB_SETSEL, 0, -1);
        for index in selected {
            if index < rows.len() {
                send(
                    list,
                    LB_SETSEL,
                    1,
                    isize::try_from(index).unwrap_or(isize::MAX),
                );
            }
        }
        if preserve {
            send(list, LB_SETTOPINDEX, usize::try_from(top).unwrap_or(0), 0);
        }
        Ok(())
    })();
    send(list, WM_SETREDRAW, 1, 0);
    let _ = unsafe { InvalidateRect(Some(list), None, false) };
    result
}

fn file_row(file: &ProductFileSnapshot) -> String {
    if file.is_directory {
        return format!("[目录]  {}", file.name);
    }
    let state = if file.state == ProductTransferState::AwaitingPaste {
        "排队中"
    } else {
        file.state.label()
    };
    let percent = progress_tenths(file.transferred, file.size, file.state);
    format!(
        "{}  {}.{}%  {} / {}  实时速度 {}    {}",
        state,
        percent / 10,
        percent % 10,
        format_bytes(file.transferred),
        format_bytes(file.size),
        transfer_speed_label(file.state, file.bytes_per_second),
        file.name
    )
}

fn terminal(state: ProductTransferState) -> bool {
    matches!(
        state,
        ProductTransferState::Completed
            | ProductTransferState::Cancelled
            | ProductTransferState::Failed
    )
}

fn task_key(id: ProductTransferId) -> String {
    format!(
        "{}:{}",
        match id.direction() {
            ProductTransferDirection::Sending => 's',
            ProductTransferDirection::Receiving => 'r',
        },
        encode_hex(&id.offer_id())
    )
}

const fn state_code(state: ProductTransferState) -> &'static str {
    match state {
        ProductTransferState::AwaitingPaste => "AwaitingPaste",
        ProductTransferState::Running => "Running",
        ProductTransferState::Paused => "Paused",
        ProductTransferState::Cancelled => "Cancelled",
        ProductTransferState::Completed => "Completed",
        ProductTransferState::Failed => "Failed",
    }
}

const fn state_rank(state: ProductTransferState) -> u8 {
    match state {
        ProductTransferState::Running => 0,
        ProductTransferState::AwaitingPaste => 1,
        ProductTransferState::Paused => 2,
        ProductTransferState::Completed => 3,
        ProductTransferState::Cancelled => 4,
        ProductTransferState::Failed => 5,
    }
}

const fn action_rank(state: ProductTransferState) -> u8 {
    match state {
        ProductTransferState::Running | ProductTransferState::AwaitingPaste => 0,
        ProductTransferState::Paused => 1,
        ProductTransferState::Completed
        | ProductTransferState::Cancelled
        | ProductTransferState::Failed => 2,
    }
}

fn send(window: HWND, message: u32, wparam: usize, lparam: isize) -> isize {
    unsafe { SendMessageW(window, message, Some(WPARAM(wparam)), Some(LPARAM(lparam))) }.0
}

fn visible(window: HWND, _show: bool) {
    // These are legacy child controls, never the WebView. Do not briefly show them
    // on every model update: their native surfaces punch through the WebView.
    let show = false;
    let style = usize::try_from(unsafe { GetWindowLongPtrW(window, GWL_STYLE) }).unwrap_or(0);
    let has_visible_style = style & WS_VISIBLE.0 as usize != 0;
    if has_visible_style != show {
        let _ = unsafe { ShowWindow(window, if show { SW_SHOWNOACTIVATE } else { SW_HIDE }) };
    }
}

fn enabled(window: HWND, enable: bool) {
    if unsafe { IsWindowEnabled(window) }.as_bool() != enable {
        let _ = unsafe { EnableWindow(window, enable) };
    }
}

fn set_text(window: HWND, text: &str) -> io::Result<()> {
    let mut existing = vec![0_u16; text.encode_utf16().count() + 2];
    let length = usize::try_from(unsafe { GetWindowTextW(window, &mut existing) }).unwrap_or(0);
    if existing[..length].iter().copied().eq(text.encode_utf16()) {
        return Ok(());
    }
    let text = wide(text);
    unsafe { SetWindowTextW(window, PCWSTR(text.as_ptr())) }.map_err(win_error)
}

fn copy_unicode_text(owner: HWND, text: &str) -> io::Result<()> {
    unsafe { OpenClipboard(Some(owner)) }.map_err(win_error)?;
    let result = (|| {
        unsafe { EmptyClipboard() }.map_err(win_error)?;
        let text = wide(text);
        let bytes = text
            .len()
            .checked_mul(std::mem::size_of::<u16>())
            .ok_or_else(|| io::Error::other("路径文本过长"))?;
        let global =
            unsafe { GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes) }.map_err(win_error)?;
        let pointer = unsafe { GlobalLock(global) };
        if pointer.is_null() {
            let _ = unsafe { GlobalFree(Some(global)) };
            return Err(io::Error::last_os_error());
        }
        unsafe {
            std::ptr::copy_nonoverlapping(text.as_ptr(), pointer.cast::<u16>(), text.len());
            let _ = GlobalUnlock(global);
        }
        if let Err(error) =
            unsafe { SetClipboardData(u32::from(CF_UNICODETEXT.0), Some(HANDLE(global.0))) }
        {
            let _ = unsafe { GlobalFree(Some(global)) };
            return Err(win_error(error));
        }
        Ok(())
    })();
    let _ = unsafe { CloseClipboard() };
    result
}

fn create_font(height: i32, weight: i32) -> io::Result<HFONT> {
    let font = unsafe {
        CreateFontW(
            height,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            u32::from(DEFAULT_PITCH.0 | FF_DONTCARE.0),
            w!("Segoe UI"),
        )
    };
    if font.0.is_null() {
        Err(io::Error::last_os_error())
    } else {
        Ok(font)
    }
}

fn progress_tenths(bytes: u64, total: u64, state: ProductTransferState) -> u16 {
    if state == ProductTransferState::Completed {
        return 1000;
    }
    u16::try_from(u128::from(bytes.min(total)) * 1000 / u128::from(total.max(1))).unwrap_or(1000)
}

pub(crate) fn transfer_speed_label(state: ProductTransferState, speed: u64) -> String {
    if state == ProductTransferState::Running {
        format!("{}/s", format_bytes(speed))
    } else {
        state.label().to_owned()
    }
}

fn format_bytes(bytes: u64) -> String {
    for (unit, label) in [(1_u64 << 30, "GiB"), (1 << 20, "MiB"), (1 << 10, "KiB")] {
        if bytes >= unit {
            return format!("{}.{} {label}", bytes / unit, bytes % unit * 10 / unit);
        }
    }
    format!("{bytes} B")
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn task_time_summary(task: &ProductTransferTaskSnapshot) -> String {
    if task.completed_unix_ms != 0 {
        return format!(
            "完成 {}  ·  用时 {}",
            format_local_time(task.completed_unix_ms),
            format_duration(task.completed_unix_ms.saturating_sub(task.started_unix_ms))
        );
    }
    if task.started_unix_ms != 0 {
        return format!(
            "开始 {}  ·  已用时 {}",
            format_local_time(task.started_unix_ms),
            format_duration(unix_time_ms().saturating_sub(task.started_unix_ms))
        );
    }
    "等待开始".to_owned()
}

fn format_duration(milliseconds: u64) -> String {
    let seconds = milliseconds / 1000;
    let hours = seconds / 3600;
    let minutes = seconds % 3600 / 60;
    let seconds = seconds % 60;
    if hours != 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

fn format_local_time(unix_ms: u64) -> String {
    const WINDOWS_EPOCH_TICKS: u64 = 116_444_736_000_000_000;
    let ticks = unix_ms
        .saturating_mul(10_000)
        .saturating_add(WINDOWS_EPOCH_TICKS);
    let utc = FILETIME {
        dwLowDateTime: u32::try_from(ticks & u64::from(u32::MAX)).unwrap_or(u32::MAX),
        dwHighDateTime: u32::try_from(ticks >> 32).unwrap_or(u32::MAX),
    };
    let mut local = FILETIME::default();
    let mut system = SYSTEMTIME::default();
    if unsafe { FileTimeToLocalFileTime(&raw const utc, &raw mut local) }.is_ok()
        && unsafe { FileTimeToSystemTime(&raw const local, &raw mut system) }.is_ok()
    {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            system.wYear, system.wMonth, system.wDay, system.wHour, system.wMinute, system.wSecond
        )
    } else {
        "时间不可用".to_owned()
    }
}

fn truncate(text: &str, count: usize) -> String {
    let prefix = text.chars().take(count).collect::<String>();
    if prefix.len() < text.len() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
fn win_error(error: windows_core::Error) -> io::Error {
    io::Error::other(error)
}

fn webview_error(error: webview2_com::Error) -> io::Error {
    io::Error::other(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard::{ProductManifestSummary, ProductTransferSnapshot};

    static TEST_WINDOW_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn window_geometry_is_clamped_without_rescaling_the_client_surface() {
        assert_eq!(
            clamp_window_geometry(
                WindowGeometry {
                    left: 1_900,
                    top: -80,
                    width: 1_400,
                    height: 1_200,
                    maximized: true,
                },
                (1_920, 1_080),
            ),
            WindowGeometry {
                left: 520,
                top: 0,
                width: 1_400,
                height: 1_080,
                maximized: true,
            }
        );
    }

    #[test]
    fn speed_labels_distinguish_zero_from_paused_and_completed() {
        assert_eq!(
            transfer_speed_label(ProductTransferState::Running, 0),
            "0 B/s"
        );
        assert_eq!(
            transfer_speed_label(ProductTransferState::Running, 8 * 1024 * 1024),
            "8.0 MiB/s"
        );
        assert_eq!(
            transfer_speed_label(ProductTransferState::Paused, 1234),
            "已暂停"
        );
        assert_eq!(
            transfer_speed_label(ProductTransferState::Completed, 1234),
            "已完成"
        );
    }

    fn task(id: u8, direction: ProductTransferDirection) -> ProductTransferTaskSnapshot {
        ProductTransferTaskSnapshot {
            id: ProductTransferId::new(direction, [id; 16]),
            generation: u64::from(id),
            manifest: ProductManifestSummary {
                direction: direction.label(),
                primary_name: "视频与资料文件夹".to_owned(),
                items: 3,
                files: 3,
                directories: 0,
                total_size: 300,
            },
            transfer: ProductTransferSnapshot {
                state: ProductTransferState::Running,
                transferred: 150,
                total_size: 300,
                bytes_per_second: 6 * 1024 * 1024,
                average_bytes_per_second: 5 * 1024 * 1024,
                started_files: 2,
                total_files: 3,
                current_file_name: Some("videoplayback.mp4".to_owned()),
                current_file_transferred: 50,
                current_file_size: 100,
                reconnect_attempts: 0,
                recovered_commands: 0,
                recovery_active: false,
            },
            files: [
                (1, "资料\\已完成.pdf", ProductTransferState::Completed, 100),
                (2, "videoplayback.mp4", ProductTransferState::Running, 50),
                (
                    3,
                    "资料\\排队中文件.zip",
                    ProductTransferState::AwaitingPaste,
                    0,
                ),
            ]
            .into_iter()
            .map(|(id, name, state, transferred)| ProductFileSnapshot {
                id: [id; 16],
                name: name.to_owned(),
                size: 100,
                transferred,
                bytes_per_second: 0,
                state,
                is_directory: false,
            })
            .collect(),
            source_location: "DESKTOP-STUDIO · C:\\资料".to_owned(),
            target_location: "本机 · 桌面".to_owned(),
            started_unix_ms: 1_788_600_000_000,
            completed_unix_ms: 0,
        }
    }

    fn hierarchy_task(id: u8) -> ProductTransferTaskSnapshot {
        let mut task = task(id, ProductTransferDirection::Receiving);
        task.manifest.primary_name = "memory_merged.mp4".to_owned();
        task.manifest.items = 6;
        task.manifest.files = 3;
        task.manifest.directories = 3;
        task.manifest.total_size = 300;
        task.files = [
            (1, "memory_merged.mp4", false, 100),
            (2, "BcutBilibili", true, 0),
            (3, "BcutBilibili\\audio", true, 0),
            (4, "BcutBilibili\\audio\\qtaudio_wasapi.dll", false, 100),
            (5, "BcutBilibili\\plugins", true, 0),
            (6, "BcutBilibili\\plugins\\avcodec-58.dll", false, 100),
        ]
        .into_iter()
        .map(|(file_id, name, is_directory, size)| ProductFileSnapshot {
            id: [file_id; 16],
            name: name.to_owned(),
            size,
            transferred: 0,
            bytes_per_second: 0,
            state: ProductTransferState::AwaitingPaste,
            is_directory,
        })
        .collect();
        // Reproduce a real transfer where a selected directory already contains a file that
        // finished before the user cancels the rest of that subtree.
        task.files[3].state = ProductTransferState::Completed;
        task.files[3].transferred = task.files[3].size;
        task
    }

    #[test]
    fn history_v1_records_remain_readable_after_full_detail_upgrade() {
        let mut completed = task(88, ProductTransferDirection::Receiving);
        completed.transfer.state = ProductTransferState::Completed;
        completed.transfer.transferred = completed.transfer.total_size;
        let legacy = [
            encode_hex(&completed.id.offer_id()),
            completed.generation.to_string(),
            history_state_code(completed.transfer.state).to_string(),
            encode_hex(completed.manifest.primary_name.as_bytes()),
            completed.manifest.items.to_string(),
            completed.manifest.files.to_string(),
            completed.manifest.directories.to_string(),
            completed.manifest.total_size.to_string(),
            completed.transfer.transferred.to_string(),
            encode_hex(completed.source_location.as_bytes()),
            encode_hex(completed.target_location.as_bytes()),
        ]
        .join("\t");
        let decoded = decode_history(&format!("{HISTORY_HEADER_V1}\n{legacy}\n")).unwrap();
        let decoded = decoded.back().unwrap();

        assert_eq!(decoded.id, completed.id);
        assert_eq!(decoded.source_location, completed.source_location);
        assert!(decoded.files.is_empty());
        assert_eq!(decoded.started_unix_ms, 0);

        let mut sending = task(89, ProductTransferDirection::Sending);
        sending.transfer.state = ProductTransferState::Completed;
        sending.transfer.transferred = sending.transfer.total_size;
        let current = decode_history(&format!(
            "{HISTORY_HEADER}\n{}\n",
            encode_history_task(&sending)
        ))
        .unwrap();
        assert_eq!(
            current.back().unwrap().id.direction(),
            ProductTransferDirection::Sending,
            "the current history format must preserve transfer direction for sortable history"
        );
    }

    #[test]
    fn first_file_shell_icon_is_captured_without_opening_its_contents() {
        let _guard = TEST_WINDOW_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut window = TransferWindow::create(
            HWND::default(),
            HICON::default(),
            TransferWindowCommands {
                pause: 1001,
                resume: 1002,
                cancel: 1003,
                dispatch: 1004,
            },
            PathBuf::new(),
            false,
        )
        .unwrap();
        let mut task = task(90, ProductTransferDirection::Sending);
        task.source_location = env!("CARGO_MANIFEST_DIR").to_owned();
        task.files[0].name = "Cargo.toml".to_owned();

        let icon = window
            .task_icon(&task)
            .expect("Cargo.toml has a Shell icon");

        assert_eq!(
            icon.key,
            format!(
                "path:{}",
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("Cargo.toml")
                    .to_string_lossy()
            )
        );
        assert_eq!((icon.width, icon.height), (48, 48));
        assert_eq!(
            icon.bgra.len(),
            usize::try_from(icon.width * icon.height * 4).unwrap()
        );
        assert!(icon.bgra.chunks_exact(4).any(|pixel| pixel[3] != 0));
    }

    #[test]
    fn transfer_view_preferences_are_validated_and_persist_outside_the_webview() {
        let _guard = TEST_WINDOW_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let history_path =
            std::env::temp_dir().join(format!("pierdesk-row-height-{}.v1", std::process::id()));
        let view_path = history_path.with_extension("view.v1");
        let _ = std::fs::remove_file(&view_path);
        let commands = TransferWindowCommands {
            pause: 1001,
            resume: 1002,
            cancel: 1003,
            dispatch: 1004,
        };
        let window = TransferWindow::create(
            HWND::default(),
            HICON::default(),
            commands,
            history_path.clone(),
            false,
        )
        .unwrap();

        window
            .state
            .handle_web_message(
                window.handle,
                r#"{"action":"set-row-height","rowHeight":88}"#,
            )
            .unwrap();
        assert_eq!(window.state.history_row_height.get(), 88);
        let widths = HistoryColumnWidths {
            name: 512,
            size: 120,
            status: 140,
            direction: 210,
            completed: 190,
            duration: 112,
            action: 132,
        };
        window
            .state
            .handle_web_message(
                window.handle,
                r#"{"action":"set-column-widths","columnWidths":{"name":512,"size":120,"status":140,"direction":210,"completed":190,"duration":112,"action":132}}"#,
            )
            .unwrap();
        assert_eq!(window.state.history_column_widths.get(), widths);
        let loaded = load_view_settings(&view_path).unwrap();
        assert_eq!(loaded.history_row_height, 88);
        assert_eq!(loaded.history_column_widths, widths);
        assert!(
            window
                .state
                .handle_web_message(
                    window.handle,
                    r#"{"action":"set-row-height","rowHeight":63}"#
                )
                .is_err()
        );
        assert_eq!(window.state.history_row_height.get(), 88);
        assert!(
            window
                .state
                .handle_web_message(
                    window.handle,
                    r#"{"action":"set-column-widths","columnWidths":{"name":100,"size":120,"status":140,"direction":210,"completed":190,"duration":112,"action":132}}"#
                )
                .is_err()
        );
        drop(window);

        let restored = TransferWindow::create(
            HWND::default(),
            HICON::default(),
            commands,
            history_path,
            false,
        )
        .unwrap();
        assert_eq!(restored.state.history_row_height.get(), 88);
        assert_eq!(restored.state.history_column_widths.get(), widths);
        drop(restored);
        let _ = std::fs::remove_file(view_path);
    }

    #[test]
    fn legacy_view_preferences_remain_readable_and_geometry_round_trips() {
        let path =
            std::env::temp_dir().join(format!("pierdesk-view-settings-{}.v1", std::process::id()));
        std::fs::write(
            &path,
            format!("{VIEW_SETTINGS_HEADER}\nhistory_row_height=84\n"),
        )
        .unwrap();
        assert_eq!(
            load_view_settings(&path).unwrap(),
            TransferViewSettings {
                history_row_height: 84,
                ..TransferViewSettings::default()
            }
        );

        let settings = TransferViewSettings {
            history_limit: 100,
            history_row_height: 72,
            history_column_widths: HistoryColumnWidths {
                name: 540,
                ..HistoryColumnWidths::default()
            },
            window_geometry: Some(WindowGeometry {
                left: 120,
                top: 80,
                width: 980,
                height: 680,
                maximized: true,
            }),
        };
        save_view_settings(&path, settings).unwrap();
        assert_eq!(load_view_settings(&path).unwrap(), settings);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn native_window_geometry_is_captured_and_restored() {
        let _dpi = crate::cross_screen::ThreadDpiAwarenessGuard::enter_per_monitor().unwrap();
        let _guard = TEST_WINDOW_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let history_path = std::env::temp_dir().join(format!(
            "pierdesk-window-geometry-{}.v1",
            std::process::id()
        ));
        let view_path = history_path.with_extension("view.v1");
        let _ = std::fs::remove_file(&view_path);
        let commands = TransferWindowCommands {
            pause: 1001,
            resume: 1002,
            cancel: 1003,
            dispatch: 1004,
        };
        let window = TransferWindow::create(
            HWND::default(),
            HICON::default(),
            commands,
            history_path.clone(),
            false,
        )
        .unwrap();
        unsafe {
            SetWindowPos(
                window.handle,
                Some(HWND_TOP),
                120,
                90,
                900,
                620,
                SWP_NOACTIVATE,
            )
        }
        .unwrap();
        window.state.persist_window_geometry(window.handle);
        let saved = load_view_settings(&view_path)
            .unwrap()
            .window_geometry
            .unwrap();
        assert_eq!(
            (saved.left, saved.top, saved.width, saved.height),
            (120, 90, 900, 620)
        );
        drop(window);

        let restored = TransferWindow::create(
            HWND::default(),
            HICON::default(),
            commands,
            history_path,
            false,
        )
        .unwrap();
        let mut rect = RECT::default();
        unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetWindowRect(restored.handle, &raw mut rect)
        }
        .unwrap();
        assert_eq!(
            (
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top
            ),
            (120, 90, 900, 620)
        );
        drop(restored);
        let _ = std::fs::remove_file(view_path);
    }

    fn evaluate_webview(window: &TransferWindow, script: &str) -> serde_json::Value {
        let webview = window
            .state
            .webview
            .borrow()
            .as_ref()
            .unwrap()
            .webview
            .clone();
        let script = wide(script);
        let (sender, receiver) = mpsc::channel();
        webview2_com::ExecuteScriptCompletedHandler::wait_for_async_operation(
            Box::new(move |handler| unsafe {
                webview
                    .ExecuteScript(PCWSTR(script.as_ptr()), &handler)
                    .map_err(webview2_com::Error::WindowsError)
            }),
            Box::new(move |error, result| {
                error?;
                sender.send(result).unwrap();
                Ok(())
            }),
        )
        .unwrap();
        serde_json::from_str(&receiver.recv().unwrap()).unwrap()
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn actual_webview_fills_resized_host_at_multiple_scales_without_legacy_controls() {
        use windows::Win32::UI::HiDpi::{
            DPI_AWARENESS_CONTEXT_UNAWARE, SetThreadDpiAwarenessContext,
        };
        let _guard = TEST_WINDOW_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _dpi = crate::cross_screen::ThreadDpiAwarenessGuard::enter_per_monitor().unwrap();
        let mut window = TransferWindow::create(
            HWND::default(),
            HICON::default(),
            TransferWindowCommands {
                pause: 1001,
                resume: 1002,
                cancel: 1003,
                dispatch: 1004,
            },
            PathBuf::new(),
            false,
        )
        .unwrap();
        window.set_language(UiLanguage::English);
        window.set_theme(UiTheme::Dark);
        let mut transfer_task = task(91, ProductTransferDirection::Receiving);
        transfer_task.source_location.clear();
        transfer_task.target_location.clear();
        let snapshot = snapshot(vec![transfer_task]);
        window.update(&snapshot).unwrap();
        window.show().unwrap();
        // Include a real WebView navigation/ready handshake, not a browser-only mock.
        for _ in 0..100 {
            if evaluate_webview(&window, "!!document.getElementById('app')").as_bool() == Some(true)
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        for _ in 0..100 {
            if evaluate_webview(
                &window,
                "document.getElementById('app').innerText.includes('Transfer Center')",
            )
            .as_bool()
                == Some(true)
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            evaluate_webview(
                &window,
                "[document.documentElement.lang,document.documentElement.dataset.theme,getComputedStyle(document.documentElement).colorScheme,getComputedStyle(document.documentElement).getPropertyValue('--canvas').trim(),document.title,document.getElementById('app').innerText.includes('Current tasks'),document.querySelector('.subtitle')?.innerText]",
            ),
            serde_json::json!([
                "en",
                "dark",
                "dark",
                "#10141c",
                "Pierdesk · Transfer Center",
                true,
                "DESKTOP-STUDIO · 1 task"
            ])
        );
        assert_eq!(
            evaluate_webview(
                &window,
                "window.__stableTransferRow = document.querySelector('tbody tr'); !!window.__stableTransferRow",
            ),
            serde_json::json!(true)
        );
        for _ in 0..12 {
            window.set_language(UiLanguage::English);
            window.set_theme(UiTheme::Dark);
        }
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            evaluate_webview(&window, "window.__stableTransferRow.isConnected"),
            serde_json::json!(true),
            "unchanged UI preferences must not replace the live transfer table"
        );
        assert_eq!(
            evaluate_webview(
                &window,
                r"[
                  englishText('历史记录保存条数'),
                  englishText('调整名称列宽'),
                  englishText('复制源位置'),
                  englishText('在资源管理器中打开目标位置'),
                  englishText('2 项 · 历史记录保留完整清单')
                ]",
            ),
            serde_json::json!([
                "History retention count",
                "Resize the Name column",
                "Copy Source",
                "Open Destination in File Explorer",
                "2 items · full list retained in history"
            ])
        );
        assert_eq!(
            evaluate_webview(
                &window,
                r"(() => {
                  document.querySelector('[data-detail]').click();
                  const rows = [...document.querySelectorAll('.path-row')];
                  const paths = rows.map(row => row.querySelector('.path-text').innerText);
                  const noOverlap = rows.every(row => {
                    const label = row.querySelector('.path-label');
                    const viewport = row.querySelector('.path-viewport');
                    const text = document.createRange();
                    text.selectNodeContents(label);
                    return text.getBoundingClientRect().right <= viewport.getBoundingClientRect().left;
                  });
                  const noChinese = rows.every(row => !/[\u3400-\u9fff]/.test(row.innerText));
                  document.querySelector('[data-back]').click();
                  return [paths, noOverlap, noChinese, state.detailKey === null];
                })()",
            ),
            serde_json::json!([
                [
                    "Path not provided by the legacy version (from DESKTOP-STUDIO)",
                    "Windows did not return an absolute destination path"
                ],
                true,
                true,
                true
            ])
        );
        assert_eq!(
            evaluate_webview(
                &window,
                r"(() => {
                  document.querySelector('[data-detail]').click();
                  const back = document.querySelector('[data-back]');
                  back.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, button: 0, pointerId: 7 }));
                  state.model.current[0].transferred += 1;
                  requestModelRender();
                  const protectedDuringPress = back.isConnected && state.renderDeferredByInteraction;
                  back.dispatchEvent(new PointerEvent('pointerup', { bubbles: true, button: 0, pointerId: 7 }));
                  back.click();
                  const table = document.querySelector('.table-scroll');
                  table.dispatchEvent(new Event('scroll'));
                  state.model.current[0].transferred += 1;
                  requestModelRender();
                  return [protectedDuringPress, state.detailKey === null, table.isConnected, state.renderDeferredByInteraction];
                })()",
            ),
            serde_json::json!([true, true, true, true])
        );
        let controller = window
            .state
            .webview
            .borrow()
            .as_ref()
            .unwrap()
            .controller
            .clone();
        let controller3: ICoreWebView2Controller3 = controller.cast().unwrap();
        unsafe { controller3.SetShouldDetectMonitorScaleChanges(false) }.unwrap();
        for scale in [1.0, 1.25, 1.5, 2.0] {
            unsafe { controller3.SetRasterizationScale(scale) }.unwrap();
            for (width, height) in [(940, 640), (1500, 820)] {
                unsafe {
                    SetWindowPos(
                        window.handle,
                        Some(HWND_TOP),
                        60,
                        60,
                        width,
                        height,
                        SWP_NOACTIVATE,
                    )
                }
                .unwrap();
                let expected = webview_bounds(window.handle).unwrap();
                // The tray WndProc can call show/resize from an unaware DPI context.
                let previous =
                    unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_UNAWARE) };
                window.state.resize_webview(window.handle);
                unsafe { SetThreadDpiAwarenessContext(previous) };
                let mut actual = RECT::default();
                unsafe { controller.Bounds(&raw mut actual) }.unwrap();
                assert_eq!(actual, expected);
                window.update(&snapshot).unwrap();
                for control in [
                    window.controls.pause_all,
                    window.controls.resume_all,
                    window.controls.cancel_all,
                    window.controls.rows[0].text,
                ] {
                    visible(control, true);
                    assert!(!unsafe { IsWindowVisible(control) }.as_bool());
                }
                assert_webview_surface_fits(&window, expected);
            }
        }
        capture_default_webview_if_requested(&window, &controller3);
        send(window.handle, WM_CLOSE, 0, 0);
        window.show().unwrap();
        let mut restored_bounds = RECT::default();
        unsafe {
            window
                .state
                .webview
                .borrow()
                .as_ref()
                .unwrap()
                .controller
                .Bounds(&raw mut restored_bounds)
        }
        .unwrap();
        assert_eq!(restored_bounds, webview_bounds(window.handle).unwrap());
        assert!(window.take_commands().is_empty());
    }

    fn capture_default_webview_if_requested(
        window: &TransferWindow,
        controller: &ICoreWebView2Controller3,
    ) {
        let Some(path) = std::env::var_os("PIERDESK_NATIVE_BOUNDS_CAPTURE") else {
            return;
        };
        let (_, _, width, height) = default_window_bounds();
        let dpi = unsafe { windows::Win32::UI::HiDpi::GetDpiForWindow(window.handle) };
        unsafe {
            controller
                .SetRasterizationScale(f64::from(dpi) / 96.0)
                .unwrap();
            SetWindowPos(
                window.handle,
                Some(HWND_TOP),
                60,
                60,
                width,
                height,
                SWP_NOACTIVATE,
            )
            .unwrap();
        }
        window.state.resize_webview(window.handle);
        settle_and_capture(window.handle, Path::new(&path)).unwrap();
    }

    fn assert_webview_surface_fits(window: &TransferWindow, expected: RECT) {
        let mut metrics = serde_json::Value::Null;
        for _ in 0..100 {
            metrics = evaluate_webview(
                window,
                "[innerWidth,innerHeight,devicePixelRatio,document.getElementById('app').getBoundingClientRect().width,document.getElementById('app').getBoundingClientRect().height]",
            );
            let value = |index| metrics[index].as_f64().unwrap();
            if (value(0) * value(2) - f64::from(expected.right)).abs() < 3.0
                && (value(1) * value(2) - f64::from(expected.bottom)).abs() < 3.0
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let value = |index| metrics[index].as_f64().unwrap();
        assert!(
            (value(0) * value(2) - f64::from(expected.right)).abs() < 3.0,
            "{metrics}"
        );
        assert!(
            (value(1) * value(2) - f64::from(expected.bottom)).abs() < 3.0,
            "{metrics}"
        );
        assert!((value(0) - value(3)).abs() < 1.0, "{metrics}");
        assert!((value(1) - value(4)).abs() < 1.0, "{metrics}");
    }

    #[test]
    fn configurable_history_limit_survives_reload_and_rejects_invalid_values() {
        let _guard = TEST_WINDOW_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let path =
            std::env::temp_dir().join(format!("pierdesk-history-limit-{}.v1", std::process::id()));
        let window = TransferWindow::create(
            HWND::default(),
            HICON::default(),
            TransferWindowCommands {
                pause: 1001,
                resume: 1002,
                cancel: 1003,
                dispatch: 1004,
            },
            path.clone(),
            false,
        )
        .unwrap();
        window
            .state
            .handle_web_message(
                window.handle,
                r#"{"action":"set-history-limit","historyLimit":40}"#,
            )
            .unwrap();
        for index in 1..=60 {
            let mut completed = task(index, ProductTransferDirection::Receiving);
            completed.transfer.state = ProductTransferState::Completed;
            window.state.remember_history(&completed);
        }
        window.state.save_history_best_effort();
        assert_eq!(load_history(&path).unwrap().len(), 40);
        assert_eq!(
            load_view_settings(&path.with_extension("view.v1"))
                .unwrap()
                .history_limit,
            40
        );
        for value in [0, 501] {
            assert!(
                window
                    .state
                    .handle_web_message(
                        window.handle,
                        &format!(r#"{{"action":"set-history-limit","historyLimit":{value}}}"#)
                    )
                    .is_err()
            );
        }
        assert_eq!(window.state.history_limit.get(), 40);
        window
            .state
            .handle_web_message(
                window.handle,
                r#"{"action":"set-history-limit","historyLimit":5}"#,
            )
            .unwrap();
        assert_eq!(load_history(&path).unwrap().len(), 5);
        assert!(window.take_commands().is_empty());
        drop(window);
        let _ = std::fs::remove_file(path.with_extension("view.v1"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn quiet_popup_default_and_close_release_only_the_webview() {
        let _guard = TEST_WINDOW_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut window = TransferWindow::create(
            HWND::default(),
            HICON::default(),
            TransferWindowCommands {
                pause: 1001,
                resume: 1002,
                cancel: 1003,
                dispatch: 1004,
            },
            PathBuf::new(),
            false,
        )
        .unwrap();
        let snapshot = snapshot(vec![task(91, ProductTransferDirection::Receiving)]);

        window.update(&snapshot).unwrap();
        assert!(!unsafe { IsWindowVisible(window.handle) }.as_bool());
        assert!(window.state.webview.borrow().is_none());
        window.show().unwrap();
        assert!(unsafe { IsWindowVisible(window.handle) }.as_bool());
        assert!(window.state.webview.borrow().is_some());

        send(window.handle, WM_CLOSE, 0, 0);

        assert!(!unsafe { IsWindowVisible(window.handle) }.as_bool());
        assert!(window.state.webview.borrow().is_none());
        assert_eq!(window.state.web_tasks.borrow().len(), 1);
        assert!(window.take_commands().is_empty());
    }

    #[test]
    fn automatic_popup_is_receive_only_and_an_empty_window_remains_openable() {
        let _guard = TEST_WINDOW_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut window = TransferWindow::create(
            HWND::default(),
            HICON::default(),
            TransferWindowCommands {
                pause: 1001,
                resume: 1002,
                cancel: 1003,
                dispatch: 1004,
            },
            PathBuf::new(),
            true,
        )
        .unwrap();
        let sending = task(94, ProductTransferDirection::Sending);

        window.update(&snapshot(vec![sending.clone()])).unwrap();
        assert!(!unsafe { IsWindowVisible(window.handle) }.as_bool());
        assert!(window.state.webview.borrow().is_none());

        let receiving = task(95, ProductTransferDirection::Receiving);
        window.update(&snapshot(vec![sending, receiving])).unwrap();
        assert!(unsafe { IsWindowVisible(window.handle) }.as_bool());
        assert!(window.state.webview.borrow().is_some());
        send(window.handle, WM_CLOSE, 0, 0);

        window.update(&snapshot(Vec::new())).unwrap();
        window.show().unwrap();
        assert!(unsafe { IsWindowVisible(window.handle) }.as_bool());
        assert!(window.state.webview.borrow().is_some());
        assert!(window.state.web_tasks.borrow().is_empty());
        assert!(window.take_commands().is_empty());

        send(window.handle, WM_CLOSE, 0, 0);
        let mut completed_receive = task(97, ProductTransferDirection::Receiving);
        completed_receive.transfer.state = ProductTransferState::Completed;
        completed_receive.transfer.transferred = completed_receive.transfer.total_size;
        completed_receive.transfer.started_files = completed_receive.transfer.total_files;
        for file in &mut completed_receive.files {
            file.state = ProductTransferState::Completed;
            file.transferred = file.size;
        }
        window
            .update(&snapshot(vec![completed_receive.clone()]))
            .unwrap();
        assert!(unsafe { IsWindowVisible(window.handle) }.as_bool());
        assert!(window.state.webview.borrow().is_some());
        assert_eq!(window.state.tasks.borrow().len(), 1);
        assert_eq!(window.state.tasks.borrow()[0].id, completed_receive.id);
        assert!(
            !window
                .state
                .archived
                .borrow()
                .contains(&completed_receive.id)
        );
    }

    #[test]
    fn completed_sending_tasks_enter_shared_history_without_becoming_current_receives() {
        let _guard = TEST_WINDOW_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let history_path = std::env::temp_dir().join(format!(
            "pierdesk-sending-history-{}.v1",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&history_path);
        let mut window = TransferWindow::create(
            HWND::default(),
            HICON::default(),
            TransferWindowCommands {
                pause: 1001,
                resume: 1002,
                cancel: 1003,
                dispatch: 1004,
            },
            history_path.clone(),
            true,
        )
        .unwrap();
        let mut sending = task(96, ProductTransferDirection::Sending);
        sending.transfer.state = ProductTransferState::Completed;
        sending.transfer.transferred = sending.transfer.total_size;
        sending.transfer.started_files = sending.transfer.total_files;

        window.update(&snapshot(vec![sending.clone()])).unwrap();

        assert!(!unsafe { IsWindowVisible(window.handle) }.as_bool());
        assert!(window.state.tasks.borrow().is_empty());
        assert!(window.state.web_tasks.borrow().is_empty());
        let history = window.state.history.borrow();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].id.direction(), ProductTransferDirection::Sending);
        assert_eq!(
            history[0].manifest.primary_name,
            sending.manifest.primary_name
        );
        drop(history);
        let persisted = load_history(&history_path).unwrap();
        assert_eq!(persisted.len(), 1);
        assert_eq!(
            persisted[0].id.direction(),
            ProductTransferDirection::Sending
        );
        drop(window);
        let _ = std::fs::remove_file(history_path.with_extension("view.v1"));
        let _ = std::fs::remove_file(history_path);
    }

    #[test]
    fn quiet_completed_receive_stays_hidden_and_enters_history() {
        let _guard = TEST_WINDOW_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut window = TransferWindow::create(
            HWND::default(),
            HICON::default(),
            TransferWindowCommands {
                pause: 1001,
                resume: 1002,
                cancel: 1003,
                dispatch: 1004,
            },
            PathBuf::new(),
            false,
        )
        .unwrap();
        let mut receiving = task(98, ProductTransferDirection::Receiving);
        receiving.transfer.state = ProductTransferState::Completed;
        receiving.transfer.transferred = receiving.transfer.total_size;
        receiving.transfer.started_files = receiving.transfer.total_files;

        window.update(&snapshot(vec![receiving.clone()])).unwrap();

        assert!(!unsafe { IsWindowVisible(window.handle) }.as_bool());
        assert!(window.state.webview.borrow().is_none());
        assert!(window.state.tasks.borrow().is_empty());
        assert!(window.state.web_tasks.borrow().is_empty());
        assert_eq!(window.state.history.borrow().len(), 1);
        assert_eq!(window.state.history.borrow()[0].id, receiving.id);
    }

    #[test]
    fn explorer_open_policy_accepts_only_the_local_side_of_each_transfer() {
        let _guard = TEST_WINDOW_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let window = TransferWindow::create(
            HWND::default(),
            HICON::default(),
            TransferWindowCommands {
                pause: 1001,
                resume: 1002,
                cancel: 1003,
                dispatch: 1004,
            },
            PathBuf::new(),
            false,
        )
        .unwrap();
        let mut receiving = task(92, ProductTransferDirection::Receiving);
        receiving.source_location = r"C:\Remote\receive-source".to_owned();
        receiving.target_location = r"C:\Local\receive-target".to_owned();
        let mut sending = task(93, ProductTransferDirection::Sending);
        sending.source_location = r"C:\Local\send-source".to_owned();
        sending.target_location = r"C:\Remote\send-target".to_owned();
        window.state.web_tasks.borrow_mut().push(receiving.clone());
        window.state.history.borrow_mut().push_back(sending.clone());

        for path in [
            &receiving.source_location,
            &receiving.target_location,
            &sending.source_location,
            &sending.target_location,
        ] {
            assert!(window.state.known_path(path));
        }
        assert!(window.state.known_local_path(&receiving.target_location));
        assert!(window.state.known_local_path(&sending.source_location));
        assert!(!window.state.known_local_path(&receiving.source_location));
        assert!(!window.state.known_local_path(&sending.target_location));

        let mut legacy = task(94, ProductTransferDirection::Sending);
        legacy.source_location = "路径未由旧版本提供（来自 笔记本）".to_owned();
        window.state.history.borrow_mut().push_back(legacy.clone());
        assert!(window.state.known_path(&legacy.source_location));
        assert!(!window.state.known_local_path(&legacy.source_location));
    }

    #[test]
    fn web_commands_reject_unknown_fields() {
        let _guard = TEST_WINDOW_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let window = TransferWindow::create(
            HWND::default(),
            HICON::default(),
            TransferWindowCommands {
                pause: 1001,
                resume: 1002,
                cancel: 1003,
                dispatch: 1004,
            },
            PathBuf::new(),
            false,
        )
        .unwrap();

        assert!(
            window
                .state
                .handle_web_message(window.handle, r#"{"action":"ready","unexpected":true}"#)
                .is_err()
        );
        assert!(!window.state.webview_ready.get());
    }

    fn click_list_row(list: HWND, index: usize) {
        let x = 280_u32;
        let y = u32::try_from(index)
            .unwrap_or(u32::MAX)
            .saturating_mul(38)
            .saturating_add(19);
        let coordinates = isize::try_from((y << 16) | x).unwrap_or(isize::MAX);
        send(list, WM_LBUTTONDOWN, 1, coordinates);
        send(list, WM_LBUTTONUP, 0, coordinates);
    }

    fn snapshot(tasks: Vec<ProductTransferTaskSnapshot>) -> ProductSnapshot {
        ProductSnapshot {
            listener_running: true,
            local_endpoint: "127.0.0.1:45233".parse().unwrap(),
            active_peer_label: "DESKTOP-STUDIO".to_owned(),
            active_peer: crate::security::CertificateFingerprint::from_bytes([7; 32]),
            auto_receive: true,
            pending_offer: false,
            last_manifest: None,
            transfer: None,
            transfer_generation: 1,
            transfer_tasks: tasks,
            shared_text_enabled: true,
            shared_text_generation: 0,
            last_shared_text: None,
            remote_boundary_workspace: None,
            last_error: None,
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn native_receiver_controls_keep_exact_targets_and_preserve_file_multiselection() {
        let _guard = TEST_WINDOW_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut window = TransferWindow::create(
            HWND::default(),
            HICON::default(),
            TransferWindowCommands {
                pause: 1001,
                resume: 1002,
                cancel: 1003,
                dispatch: 1004,
            },
            PathBuf::new(),
            true,
        )
        .unwrap();
        let sending = task(1, ProductTransferDirection::Sending);
        window.update(&snapshot(vec![sending.clone()])).unwrap();
        assert!(!unsafe { IsWindowVisible(window.handle) }.as_bool());
        let first = task(2, ProductTransferDirection::Receiving);
        let second = task(3, ProductTransferDirection::Receiving);
        window
            .update(&snapshot(vec![sending.clone(), first.clone()]))
            .unwrap();
        let mut single_bounds = RECT::default();
        unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetWindowRect(
                window.handle,
                &raw mut single_bounds,
            )
        }
        .unwrap();
        let snap = snapshot(vec![sending, first.clone(), second.clone()]);
        window.update(&snap).unwrap();
        let mut double_bounds = RECT::default();
        unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetWindowRect(
                window.handle,
                &raw mut double_bounds,
            )
        }
        .unwrap();
        assert_eq!(
            double_bounds.bottom - double_bounds.top,
            single_bounds.bottom - single_bounds.top,
            "the resizable transfer center must not jump in size as rows arrive"
        );
        let style = usize::try_from(unsafe { GetWindowLongPtrW(window.handle, GWL_STYLE) })
            .unwrap_or_default();
        assert_ne!(
            style & 0x000f_0000,
            0,
            "the transfer center must expose the standard system, resize, minimize and maximize styles"
        );
        assert_eq!(window_text(window.controls.rows[0].pause), "暂停此任务");
        assert!(unsafe { IsWindowVisible(window.handle) }.as_bool());
        assert_eq!(window.state.tasks.borrow().len(), 2);
        assert_eq!(window.presented.len(), 2);
        window
            .state
            .handle_web_message(
                window.handle,
                &format!(r#"{{"action":"pause","task":"{}"}}"#, task_key(first.id)),
            )
            .unwrap();
        assert_eq!(
            window.take_commands(),
            vec![ProductCommand::PauseTask(first.id)]
        );
        window
            .state
            .handle_web_message(
                window.handle,
                &format!(r#"{{"action":"cancel","task":"{}"}}"#, task_key(first.id)),
            )
            .unwrap();
        assert_eq!(
            window.take_commands(),
            vec![ProductCommand::CancelTask(first.id)]
        );
        assert!(
            unsafe { IsWindowVisible(window.handle) }.as_bool(),
            "cancelling a task must not hide"
        );
        send(window.handle, WM_COMMAND, ROW_BASE + 5, 0);
        assert_eq!(
            window.take_commands(),
            vec![ProductCommand::PauseTask(second.id)]
        );
        send(window.handle, WM_COMMAND, HIDE, 0);
        window.update(&snap).unwrap();
        assert!(
            !unsafe { IsWindowVisible(window.handle) }.as_bool(),
            "the same batch must stay hidden"
        );
        window.show().unwrap();
        send(window.handle, WM_COMMAND, ROW_BASE | (1 << 16), 0); // STN_DBLCLK
        window.update(&snap).unwrap();
        assert_eq!(window.state.details.get(), Some(first.id));
        send(window.controls.files, LB_SETSEL, 1, 1);
        send(window.controls.files, LB_SETSEL, 1, 2);
        let mut changed = snap.clone();
        changed.transfer_tasks[1].files[1].transferred = 60;
        window.update(&changed).unwrap();
        if let Some(path) = std::env::var_os("PIERDESK_TRANSFER_UI_CAPTURE_DETAILS") {
            settle_and_capture(window.handle, std::path::Path::new(&path)).unwrap();
        }
        assert_eq!(send(window.controls.files, LB_GETSEL, 1, 0), 1);
        assert_eq!(send(window.controls.files, LB_GETSEL, 2, 0), 1);
        send(window.handle, WM_COMMAND, FILE_CANCEL, 0);
        assert_eq!(
            window.take_commands(),
            vec![ProductCommand::CancelFiles(
                first.id,
                vec![[2; 16], [3; 16]]
            )]
        );
        assert!(unsafe { IsWindowVisible(window.handle) }.as_bool());
        // Optional isolated visual review. No clipboard, sockets, pairing or real files are used.
        if std::env::var_os("PIERDESK_TRANSFER_UI_PREVIEW").is_some()
            || std::env::var_os("PIERDESK_TRANSFER_UI_CAPTURE").is_some()
            || std::env::var_os("PIERDESK_TRANSFER_UI_CAPTURE_HOVER").is_some()
        {
            send(window.handle, WM_COMMAND, BACK, 0);
            window.update(&changed).unwrap();
        }
        if let Some(path) = std::env::var_os("PIERDESK_TRANSFER_UI_CAPTURE_HOVER") {
            send(window.controls.rows[0].pause, WM_MOUSEMOVE, 0, 0);
            settle_and_capture(window.handle, std::path::Path::new(&path)).unwrap();
            send(window.controls.rows[0].pause, WM_MOUSELEAVE, 0, 0);
        }
        if let Some(path) = std::env::var_os("PIERDESK_TRANSFER_UI_CAPTURE") {
            settle_and_capture(window.handle, std::path::Path::new(&path)).unwrap();
        }
        if std::env::var_os("PIERDESK_TRANSFER_UI_PREVIEW").is_some() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
            while std::time::Instant::now() < deadline {
                use windows::Win32::UI::WindowsAndMessaging::{
                    DispatchMessageW, PM_REMOVE, PeekMessageW, TranslateMessage,
                };
                let mut message = MSG::default();
                while unsafe { PeekMessageW(&raw mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                    unsafe {
                        let _ = TranslateMessage(&raw const message);
                        DispatchMessageW(&raw const message);
                    }
                }
                window.update(&changed).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
        let mut cancelled = first;
        cancelled.transfer.state = ProductTransferState::Cancelled;
        window.update(&snapshot(vec![cancelled])).unwrap();
        assert!(window.state.tasks.borrow().is_empty());
        assert!(
            unsafe { IsWindowVisible(window.handle) }.as_bool(),
            "an empty queue must keep an explicitly opened transfer window available"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn directory_selection_cascades_to_descendant_files_and_task_title_lists_roots() {
        let _guard = TEST_WINDOW_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut window = TransferWindow::create(
            HWND::default(),
            HICON::default(),
            TransferWindowCommands {
                pause: 1001,
                resume: 1002,
                cancel: 1003,
                dispatch: 1004,
            },
            PathBuf::new(),
            true,
        )
        .unwrap();
        let task = hierarchy_task(9);
        assert_eq!(task_display_name(&task), "memory_merged.mp4、BcutBilibili");
        let snapshot = snapshot(vec![task.clone()]);
        window.update(&snapshot).unwrap();
        send(window.controls.rows[0].details, 0x00f5, 0, 0); // BM_CLICK
        window.update(&snapshot).unwrap();
        assert_eq!(window.state.details.get(), Some(task.id));

        let list_style =
            usize::try_from(unsafe { GetWindowLongPtrW(window.controls.files, GWL_STYLE) })
                .unwrap_or_default();
        assert_ne!(
            list_style & usize::try_from(LBS_NOTIFY_STYLE).unwrap_or_default(),
            0,
            "the list box must notify its parent about real mouse selection"
        );
        click_list_row(window.controls.files, 1);
        assert_eq!(send(window.controls.files, LB_GETSEL, 0, 0), 0);
        for index in 1..task.files.len() {
            assert_eq!(
                send(window.controls.files, LB_GETSEL, index, 0),
                1,
                "directory descendant at index {index} must be selected"
            );
        }
        if let Some(path) = std::env::var_os("PIERDESK_TRANSFER_UI_CAPTURE_TREE_SELECTED") {
            settle_and_capture(window.handle, std::path::Path::new(&path)).unwrap();
        }
        send(window.controls.files, LB_SETSEL, 0, 1);
        send(
            window.handle,
            WM_COMMAND,
            FILE_LIST | (LBN_SELCHANGE << 16),
            0,
        );
        for index in 1..task.files.len() {
            assert_eq!(
                send(window.controls.files, LB_GETSEL, index, 0),
                0,
                "deselecting a directory must clear every descendant"
            );
        }
        click_list_row(window.controls.files, 1);
        window.state.confirm_cancel = |state, _, files, directories| {
            assert_eq!(
                files, 1,
                "completed descendants are not cancellation targets"
            );
            assert_eq!(directories, 3);
            assert!(state.file_ids.try_borrow_mut().is_ok());
            assert!(state.file_items.try_borrow_mut().is_ok());
            false
        };
        send(window.handle, WM_COMMAND, FILE_CANCEL, 0);
        assert!(window.take_commands().is_empty());
        window.update(&snapshot).unwrap();
        assert_eq!(
            window.state.file_items.borrow().len(),
            task.files.len(),
            "rejecting the confirmation must leave the queue untouched"
        );
        window.state.confirm_cancel = |state, _, files, directories| {
            assert_eq!(files, 1);
            assert_eq!(directories, 3);
            assert!(
                state.file_ids.try_borrow_mut().is_ok(),
                "modal confirmation must not retain a file-id borrow"
            );
            assert!(
                state.file_items.try_borrow_mut().is_ok(),
                "modal confirmation must not retain a file-item borrow"
            );
            true
        };
        send(window.handle, WM_COMMAND, FILE_CANCEL, 0);
        assert_eq!(
            window.take_commands(),
            vec![ProductCommand::CancelFiles(task.id, vec![[6; 16]])],
            "directory rows and completed files are not sent to the cancellation layer"
        );
        window.update(&snapshot).unwrap();
        assert_eq!(window.state.file_items.borrow().len(), 1);
        assert_eq!(
            window.state.file_items.borrow()[0].name,
            "memory_merged.mp4",
            "confirmed directory cancellation must remove its full subtree from the queue"
        );
        if let Some(path) = std::env::var_os("PIERDESK_TRANSFER_UI_CAPTURE_TREE") {
            settle_and_capture(window.handle, std::path::Path::new(&path)).unwrap();
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn finished_tasks_archive_on_hide_and_history_is_bounded_and_durable() {
        let _guard = TEST_WINDOW_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let history_path = std::env::temp_dir().join(format!(
            "pierdesk-transfer-history-{}.v1",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&history_path);
        let mut window = TransferWindow::create(
            HWND::default(),
            HICON::default(),
            TransferWindowCommands {
                pause: 1001,
                resume: 1002,
                cancel: 1003,
                dispatch: 1004,
            },
            history_path.clone(),
            true,
        )
        .unwrap();
        let empty = snapshot(Vec::new());
        window.update(&empty).unwrap();
        window.show().unwrap();
        assert!(unsafe { IsWindowVisible(window.handle) }.as_bool());
        assert!(window.state.tasks.borrow().is_empty());
        if let Some(path) = std::env::var_os("PIERDESK_TRANSFER_UI_CAPTURE_EMPTY") {
            settle_and_capture(window.handle, std::path::Path::new(&path)).unwrap();
        }

        let running = task(40, ProductTransferDirection::Receiving);
        window.update(&snapshot(vec![running.clone()])).unwrap();
        send(window.handle, WM_COMMAND, HIDE, 0);
        let mut completed_while_hidden = running;
        completed_while_hidden.transfer.state = ProductTransferState::Completed;
        completed_while_hidden.transfer.transferred = completed_while_hidden.transfer.total_size;
        window
            .update(&snapshot(vec![completed_while_hidden]))
            .unwrap();
        assert!(window.state.tasks.borrow().is_empty());
        assert_eq!(window.state.history.borrow().len(), 1);
        window.show().unwrap();

        let mut completed = task(41, ProductTransferDirection::Receiving);
        completed.transfer.state = ProductTransferState::Completed;
        completed.transfer.transferred = completed.transfer.total_size;
        completed.transfer.started_files = completed.transfer.total_files;
        let completed_snapshot = snapshot(vec![completed.clone()]);
        window.update(&completed_snapshot).unwrap();
        assert_eq!(window.state.tasks.borrow().len(), 1);
        assert_eq!(window.state.history.borrow().len(), 2);

        send(window.handle, WM_COMMAND, HIDE, 0);
        window.update(&completed_snapshot).unwrap();
        assert!(window.state.tasks.borrow().is_empty());
        window.show().unwrap();
        assert!(unsafe { IsWindowVisible(window.handle) }.as_bool());
        for id in 42..=46 {
            let mut finished = task(id, ProductTransferDirection::Receiving);
            finished.transfer.state = ProductTransferState::Completed;
            finished.transfer.transferred = finished.transfer.total_size;
            finished.completed_unix_ms = finished.started_unix_ms + 5_000;
            assert!(window.state.remember_history(&finished));
        }
        send(window.controls.history, 0x00f5, 0, 0); // BM_CLICK
        window.update(&completed_snapshot).unwrap();
        assert!(window.state.history_mode.get());
        assert_eq!(window.state.tasks.borrow().len(), ROWS);
        if let Some(path) = std::env::var_os("PIERDESK_TRANSFER_UI_CAPTURE_HISTORY_LIST") {
            settle_and_capture(window.handle, std::path::Path::new(&path)).unwrap();
        }
        send(window.controls.rows[0].details, 0x00f5, 0, 0); // BM_CLICK
        window.update(&completed_snapshot).unwrap();
        assert!(window_text(window.controls.source_location).starts_with("源位置："));
        assert!(window_text(window.controls.target_location).starts_with("目标位置："));
        assert_eq!(
            window.state.file_items.borrow().len(),
            completed.files.len(),
            "history details must preserve the complete file list"
        );
        assert!(
            window_text(window.controls.time_info).contains("完成"),
            "history details must display completion time and duration"
        );
        if let Some(path) = std::env::var_os("PIERDESK_TRANSFER_UI_CAPTURE_HISTORY") {
            settle_and_capture(window.handle, std::path::Path::new(&path)).unwrap();
        }

        for id in 47..=70 {
            let mut finished = task(id, ProductTransferDirection::Receiving);
            finished.transfer.state = ProductTransferState::Completed;
            finished.transfer.transferred = finished.transfer.total_size;
            finished.completed_unix_ms = finished.started_unix_ms + 5_000;
            assert!(window.state.remember_history(&finished));
        }
        window.state.save_history_best_effort();
        assert_eq!(window.state.history.borrow().len(), HISTORY_LIMIT);
        let loaded = load_history(&history_path).unwrap();
        assert_eq!(loaded.len(), HISTORY_LIMIT);
        assert_eq!(
            loaded.back().unwrap().id,
            ProductTransferId::new(ProductTransferDirection::Receiving, [70; 16])
        );
        assert_eq!(loaded.back().unwrap().files.len(), 3);
        assert_ne!(loaded.back().unwrap().started_unix_ms, 0);
        drop(window);
        let _ = std::fs::remove_file(history_path);
    }

    fn settle_and_capture(window: HWND, path: &std::path::Path) -> io::Result<()> {
        use windows::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, PM_REMOVE, PeekMessageW, SWP_NOSIZE, TranslateMessage,
        };
        unsafe {
            SetWindowPos(
                window,
                Some(HWND_TOP),
                100,
                100,
                0,
                0,
                SWP_NOSIZE | SWP_NOACTIVATE,
            )
        }
        .map_err(win_error)?;
        let paint_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < paint_deadline {
            let mut message = MSG::default();
            while unsafe { PeekMessageW(&raw mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                unsafe {
                    let _ = TranslateMessage(&raw const message);
                    DispatchMessageW(&raw const message);
                }
            }
            std::thread::yield_now();
        }
        assert!(
            unsafe { IsWindowVisible(window) }.as_bool(),
            "the native transfer host must stay visible during visual capture"
        );
        let state = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *const ProcedureState;
        assert!(
            !state.is_null() && unsafe { &*state }.webview_ready.get(),
            "the embedded WebView2 page must finish its ready handshake before visual capture"
        );
        capture_window_bmp(window, path)
    }

    fn capture_window_bmp(window: HWND, path: &std::path::Path) -> io::Result<()> {
        use windows::Win32::Graphics::Gdi::{
            BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap,
            CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, GetDC, GetDIBits, HBITMAP, ReleaseDC,
            SRCCOPY,
        };
        use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

        let mut rect = RECT::default();
        unsafe { GetWindowRect(window, &raw mut rect) }.map_err(win_error)?;
        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        // WebView2 uses DirectComposition and is not present in a BitBlt from the host window DC.
        // Capture the on-screen rectangle so the optional visual QA image includes the browser
        // child as the user actually sees it.
        let source = unsafe { GetDC(None) };
        let memory = unsafe { CreateCompatibleDC(Some(source)) };
        let bitmap: HBITMAP = unsafe { CreateCompatibleBitmap(source, width, height) };
        if source.0.is_null() || memory.0.is_null() || bitmap.0.is_null() {
            return Err(io::Error::last_os_error());
        }
        let previous = unsafe { SelectObject(memory, HGDIOBJ(bitmap.0)) };
        unsafe {
            BitBlt(
                memory,
                0,
                0,
                width,
                height,
                Some(source),
                rect.left,
                rect.top,
                SRCCOPY,
            )
        }
        .map_err(win_error)?;
        let _ = unsafe { SelectObject(memory, previous) };
        let image_bytes = usize::try_from(width)
            .ok()
            .and_then(|width| {
                usize::try_from(height)
                    .ok()
                    .map(|height| width * height * 4)
            })
            .ok_or_else(|| io::Error::other("invalid preview dimensions"))?;
        let mut pixels = vec![0_u8; image_bytes];
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: u32::try_from(std::mem::size_of::<BITMAPINFOHEADER>()).unwrap_or(40),
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                biSizeImage: u32::try_from(image_bytes).unwrap_or(u32::MAX),
                ..Default::default()
            },
            ..Default::default()
        };
        let lines = unsafe {
            GetDIBits(
                source,
                bitmap,
                0,
                u32::try_from(height).unwrap_or(0),
                Some(pixels.as_mut_ptr().cast()),
                &raw mut info,
                DIB_RGB_COLORS,
            )
        };
        let _ = unsafe { DeleteObject(HGDIOBJ(bitmap.0)) };
        let _ = unsafe { DeleteDC(memory) };
        unsafe { ReleaseDC(None, source) };
        if lines != height {
            return Err(io::Error::last_os_error());
        }
        let file_size = 54_u32.saturating_add(u32::try_from(image_bytes).unwrap_or(u32::MAX));
        let mut bmp = Vec::with_capacity(54 + image_bytes);
        bmp.extend_from_slice(b"BM");
        bmp.extend_from_slice(&file_size.to_le_bytes());
        bmp.extend_from_slice(&[0; 4]);
        bmp.extend_from_slice(&54_u32.to_le_bytes());
        bmp.extend_from_slice(&40_u32.to_le_bytes());
        bmp.extend_from_slice(&width.to_le_bytes());
        bmp.extend_from_slice(&(-height).to_le_bytes());
        bmp.extend_from_slice(&1_u16.to_le_bytes());
        bmp.extend_from_slice(&32_u16.to_le_bytes());
        bmp.extend_from_slice(&BI_RGB.0.to_le_bytes());
        bmp.extend_from_slice(&u32::try_from(image_bytes).unwrap_or(u32::MAX).to_le_bytes());
        bmp.extend_from_slice(&[0; 16]);
        bmp.extend_from_slice(&pixels);
        std::fs::write(path, bmp)
    }
}
