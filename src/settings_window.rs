use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::ffi::c_void;
use std::fs;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

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
use windows::Win32::Foundation::{E_POINTER, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CS_DBLCLKS, CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA,
    GetClientRect, GetSystemMetrics, GetWindowLongPtrW, GetWindowPlacement, GetWindowRect, HICON,
    IDC_ARROW, IsDialogMessageW, IsZoomed, LoadCursorW, MINMAXINFO, MSG, PostMessageW,
    RegisterClassW, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_HIDE, SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE,
    SendMessageW, SetForegroundWindow, SetWindowLongPtrW, SetWindowTextW, ShowWindow,
    UnregisterClassW, WINDOW_EX_STYLE, WINDOWPLACEMENT, WM_CLOSE, WM_EXITSIZEMOVE,
    WM_GETMINMAXINFO, WM_NCCALCSIZE, WM_NCCREATE, WM_NCDESTROY, WM_NCHITTEST, WM_NCLBUTTONDOWN,
    WM_SIZE, WM_WINDOWPOSCHANGED, WNDCLASSW, WS_CLIPCHILDREN, WS_MAXIMIZEBOX, WS_MINIMIZEBOX,
    WS_POPUP, WS_SYSMENU, WS_THICKFRAME,
};
use windows_core::{Interface as _, PCWSTR, PWSTR, w};

const CLASS: PCWSTR = w!("PierdeskSettingsWindow");
const WEB_CONTENT: &str = include_str!("../assets/settings_window/index.html");
const ICON_BYTES: &[u8] = include_bytes!("../assets/brand/pierdesk-icon-512.png");
#[cfg(feature = "licensing")]
const LICENSE_JS: &str = include_str!("../assets/settings_window/license.js");
#[cfg(not(feature = "licensing"))]
const LICENSE_JS: &str = include_str!("../assets/settings_window/license_stub.js");
// Only the community build offers "Support the author"; official (licensing) builds carry no donation codes.
#[cfg(not(feature = "licensing"))]
const WECHAT_SUPPORT_BYTES: &[u8] = include_bytes!("../assets/support/wechat-pay-donation.jpg");
#[cfg(not(feature = "licensing"))]
const ALIPAY_SUPPORT_BYTES: &[u8] = include_bytes!("../assets/support/alipay-donation.jpg");
const WINDOW_MIN_WIDTH: i32 = 940;
const WINDOW_MIN_HEIGHT: i32 = 680;
const WINDOW_WIDTH: i32 = 1540;
const WINDOW_HEIGHT: i32 = 1000;
const RESIZE_BORDER: i32 = 8;
const TITLEBAR_HEIGHT: i32 = 72;
const TITLEBAR_BUTTONS_WIDTH: i32 = 148;
const WEB_MESSAGE_LIMIT: usize = 64 * 1024;
const WINDOW_BOUNDS_FILE: &str = "settings-window.v1";
const WINDOW_BOUNDS_HEADER: &str = "ClipFerrySettingsWindowV1";
const BOUNDARY_GRAPH_FILE: &str = "boundary-graph.v1.json";
const BOUNDARY_GRAPH_LIMIT: u64 = 4 * 1024;
const MAX_BOUNDARY_GRAPH_NODES: usize = 5;

#[derive(Clone, Copy)]
pub struct SettingsWindowCommands {
    pub dispatch: usize,
}

#[cfg(feature = "licensing")]
#[derive(Clone, Eq, PartialEq)]
pub struct ActivationCode(pub zeroize::Zeroizing<String>);

#[cfg(feature = "licensing")]
impl std::fmt::Debug for ActivationCode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ActivationCode([redacted])")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SettingsWindowCommand {
    #[cfg(feature = "licensing")]
    LicenseActivate(ActivationCode),
    #[cfg(feature = "licensing")]
    LicenseRefresh,
    #[cfg(feature = "licensing")]
    LicenseRelease,
    Ssh(crate::ssh::Command),
    Refresh,
    Pair(String),
    ApprovePairing(bool),
    Revoke(String),
    SelectPeer(String),
    SetLayout {
        fingerprint: String,
        placement: String,
    },
    SaveBoundaryMap(serde_json::Value),
    SaveBoundaryWorkspaceV3 {
        expected_storage_revision: u64,
        layout: serde_json::Value,
        surface: String,
    },
    PreviewBoundaryWorkspaceV3 {
        layout: serde_json::Value,
    },
    AcceptBoundaryWorkspacePreview {
        sequence: u64,
    },
    ApplyBoundaryWorkspaceV3 {
        expected_storage_revision: u64,
    },
    SaveConnection {
        active_peer: String,
        local_endpoint: String,
        peer_endpoint: String,
        auto_receive: bool,
    },
    SetCrossScreenEnabled(bool),
    SetPairEnabled {
        fingerprint: String,
        enabled: bool,
    },
    SetBoundaryMapV2Enabled(bool),
    SetPlacement(String),
    SetCornerGuardEnabled(bool),
    SetCornerGuardPixels(u32),
    SetSecondPush(bool),
    SetFullscreenPause(bool),
    SetKeepLinkWarm(bool),
    PauseCrossScreen,
    ResumeCrossScreen,
    ReturnLocal,
    RestartElevated,
    ShowCompatibility,
    SetSharedClipboardEnabled(bool),
    Screenshot,
    SetScreenshotShortcut(String),
    OpenTransfer {
        history: bool,
    },
    SetTransferAutoPopup(bool),
    AcceptPending,
    PauseAll,
    ResumeAll,
    CancelAll,
    PauseTask(String),
    CancelTask(String),
    SetHistoryLimit(usize),
    SetHistoryRowHeight(u16),
    SetAutostart(bool),
    SetLanguage(String),
    SetTheme(String),
    SetFontScale(u16),
    OpenLog,
    OpenLogFolder,
    OpenSource,
    Exit,
}

struct ProcedureState {
    target: HWND,
    commands: SettingsWindowCommands,
    hidden: Cell<bool>,
    webview: RefCell<Option<WebViewHost>>,
    webview_ready: Cell<bool>,
    webview_user_data: PathBuf,
    window_bounds_path: PathBuf,
    boundary_graph_path: PathBuf,
    restore_maximized: Cell<bool>,
    html: String,
    model_json: RefCell<String>,
    page: RefCell<String>,
    pending: RefCell<VecDeque<SettingsWindowCommand>>,
}

pub struct SettingsWindow {
    handle: HWND,
    instance: HINSTANCE,
    state: Box<ProcedureState>,
    _com: ComApartment,
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct WebCommand {
    action: String,
    #[serde(default)]
    shortcut: Option<String>,
    #[serde(default)]
    #[cfg(feature = "licensing")]
    activation_code: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    sid: Option<String>,
    #[serde(default)]
    expected: Option<String>,
    #[serde(default)]
    fingerprint: Option<String>,
    #[serde(default)]
    active_peer: Option<String>,
    #[serde(default)]
    local_endpoint: Option<String>,
    #[serde(default)]
    peer_endpoint: Option<String>,
    #[serde(default)]
    auto_receive: Option<bool>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    placement: Option<String>,
    #[serde(default)]
    boundary_map: Option<serde_json::Value>,
    #[serde(default)]
    boundary_layout: Option<serde_json::Value>,
    #[serde(default)]
    surface: Option<String>,
    #[serde(default)]
    expected_storage_revision: Option<u64>,
    #[serde(default)]
    sequence: Option<u64>,
    #[serde(default)]
    pixels: Option<u32>,
    #[serde(default)]
    value: Option<u64>,
    #[serde(default)]
    task: Option<String>,
    #[serde(default)]
    history: Option<bool>,
    #[serde(default)]
    edge: Option<String>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    theme: Option<String>,
    #[serde(default)]
    graph_positions: Option<BTreeMap<String, BoundaryGraphPoint>>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct BoundaryGraphPoint {
    x: f64,
    y: f64,
}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SavedBoundaryGraph {
    schema_version: u8,
    positions: BTreeMap<String, BoundaryGraphPoint>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SavedWindowBounds {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    maximized: bool,
}

impl SettingsWindow {
    pub fn create(
        target: HWND,
        icon: HICON,
        commands: SettingsWindowCommands,
        data_root: &Path,
    ) -> io::Result<Self> {
        let _dpi_guard = crate::cross_screen::ThreadDpiAwarenessGuard::enter_per_monitor()?;
        let com = ComApartment::enter()?;
        let instance = HINSTANCE(unsafe { GetModuleHandleW(None) }.map_err(win_error)?.0);
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

        let window_bounds_path = data_root.join(WINDOW_BOUNDS_FILE);
        let boundary_graph_path = data_root.join(BOUNDARY_GRAPH_FILE);
        let saved_bounds = load_window_bounds(&window_bounds_path).ok();
        let bounds = saved_bounds.map_or_else(default_window_bounds, clamp_window_bounds);
        let (x, y, width, height) = (bounds.x, bounds.y, bounds.width, bounds.height);
        let icon_data = format!("data:image/png;base64,{}", encode_base64(ICON_BYTES));
        #[cfg(not(feature = "licensing"))]
        let (wechat_support_data, alipay_support_data) = (
            format!(
                "data:image/jpeg;base64,{}",
                encode_base64(WECHAT_SUPPORT_BYTES)
            ),
            format!(
                "data:image/jpeg;base64,{}",
                encode_base64(ALIPAY_SUPPORT_BYTES)
            ),
        );
        #[cfg(feature = "licensing")]
        let (wechat_support_data, alipay_support_data) = (String::new(), String::new());
        let html = WEB_CONTENT
            .replace(
                "/*__LICENSE_JS__*/",
                LICENSE_JS,
            )
            .replace("__PIERDESK_ICON__", &icon_data)
            .replace("__WECHAT_SUPPORT_QR__", &wechat_support_data)
            .replace("__ALIPAY_SUPPORT_QR__", &alipay_support_data)
            .replace(
                "/*__BOUNDARY_EDITOR_CSS__*/",
                include_str!("../assets/settings_window/boundary_editor.css"),
            )
            .replace(
                "/*__SSH_CSS__*/",
                include_str!("../assets/settings_window/ssh.css"),
            )
            .replace(
                "/*__BOUNDARY_EDITOR_JS__*/",
                include_str!("../assets/settings_window/boundary_editor.js"),
            )
            .replace(
                "/*__SSH_JS__*/",
                include_str!("../assets/settings_window/ssh.js"),
            );
        let mut state = Box::new(ProcedureState {
            target,
            commands,
            hidden: Cell::new(true),
            webview: RefCell::new(None),
            webview_ready: Cell::new(false),
            webview_user_data: data_root.join("WebView2"),
            window_bounds_path,
            boundary_graph_path,
            restore_maximized: Cell::new(saved_bounds.is_some_and(|value| value.maximized)),
            html,
            model_json: RefCell::new(String::new()),
            page: RefCell::new("status".to_owned()),
            pending: RefCell::new(VecDeque::new()),
        });
        let style = WS_POPUP
            | WS_THICKFRAME
            | WS_MINIMIZEBOX
            | WS_MAXIMIZEBOX
            | WS_SYSMENU
            | WS_CLIPCHILDREN;
        let handle = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                CLASS,
                w!("Pierdesk Settings"),
                style,
                x,
                y,
                width,
                height,
                None,
                None,
                Some(instance),
                Some((&raw mut *state).cast::<c_void>()),
            )
        }
        .map_err(win_error)?;
        Ok(Self {
            handle,
            instance,
            state,
            _com: com,
        })
    }

    pub fn show(&self, page: &str) -> io::Result<()> {
        let page = validate_page(page);
        self.state.page.replace(page.to_owned());
        self.state.hidden.set(false);
        self.state.ensure_webview(self.handle)?;
        unsafe {
            let _ = ShowWindow(
                self.handle,
                if self.state.restore_maximized.get() {
                    SW_MAXIMIZE
                } else {
                    SW_RESTORE
                },
            );
            let _ = SetForegroundWindow(self.handle);
        }
        self.state.resize_webview(self.handle);
        self.state.post_model()?;
        self.state.post_navigation(page)
    }

    pub fn update_json(&self, json: String) -> io::Result<()> {
        if *self.state.model_json.borrow() == json {
            return Ok(());
        }
        let language = serde_json::from_str::<serde_json::Value>(&json)
            .ok()
            .and_then(|value| value.get("language")?.as_str().map(str::to_owned));
        let title = if language.as_deref() == Some("zh-CN") {
            "Pierdesk 设置"
        } else {
            "Pierdesk Settings"
        };
        let title = wide(title);
        unsafe { SetWindowTextW(self.handle, PCWSTR(title.as_ptr())) }.map_err(win_error)?;
        self.state.model_json.replace(json);
        self.state.post_model()
    }

    pub fn take_commands(&self) -> Vec<SettingsWindowCommand> {
        self.state.pending.borrow_mut().drain(..).collect()
    }

    pub fn is_visible(&self) -> bool {
        !self.state.hidden.get()
    }

    pub fn toast(&self, message: &str) {
        let payload = serde_json::json!({ "type": "toast", "message": message }).to_string();
        let _ = self.state.post_json(&payload);
    }

    pub fn boundary_save_result(&self, success: bool, message: &str) {
        let payload = serde_json::json!({
            "type": "boundary-save-result",
            "success": success,
            "message": message
        })
        .to_string();
        let _ = self.state.post_json(&payload);
    }

    pub fn is_dialog_message(&self, message: &MSG) -> bool {
        unsafe { IsDialogMessageW(self.handle, message) }.as_bool()
    }
}

impl Drop for SettingsWindow {
    fn drop(&mut self) {
        self.state.close_webview();
        unsafe {
            let _ = DestroyWindow(self.handle);
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
        let html = wide(&self.html);
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
            let _ = unsafe { host.controller.NotifyParentWindowPositionChanged() };
        }
    }

    fn post_model(&self) -> io::Result<()> {
        if !self.webview_ready.get() || self.model_json.borrow().is_empty() {
            return Ok(());
        }
        self.post_json(&self.model_json.borrow())
    }

    fn post_navigation(&self, page: &str) -> io::Result<()> {
        let payload = serde_json::json!({ "type": "navigate", "page": page }).to_string();
        self.post_json(&payload)
    }

    fn post_boundary_graph(&self) -> io::Result<()> {
        let positions = match load_boundary_graph(&self.boundary_graph_path) {
            Ok(graph) => graph.positions,
            Err(error) if error.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(_) => BTreeMap::new(),
        };
        let payload = serde_json::json!({
            "type": "boundary-graph-state",
            "positions": positions,
        })
        .to_string();
        self.post_json(&payload)
    }

    fn post_json(&self, json: &str) -> io::Result<()> {
        if !self.webview_ready.get() {
            return Ok(());
        }
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

    fn enqueue(&self, command: SettingsWindowCommand) {
        self.pending.borrow_mut().push_back(command);
        let _ = unsafe {
            PostMessageW(
                Some(self.target),
                windows::Win32::UI::WindowsAndMessaging::WM_COMMAND,
                WPARAM(self.commands.dispatch),
                LPARAM(0),
            )
        };
    }

    #[allow(clippy::too_many_lines)]
    fn handle_web_message(&self, window: HWND, json: &str) -> io::Result<()> {
        if json.len() > WEB_MESSAGE_LIMIT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "设置窗口命令过长",
            ));
        }
        let command: WebCommand = serde_json::from_str(json).map_err(io::Error::other)?;
        match command.action.as_str() {
            "ready" => {
                self.webview_ready.set(true);
                self.post_model()?;
                self.post_navigation(&self.page.borrow())?;
                self.post_boundary_graph()?;
            }
            "window-minimize" => unsafe {
                let _ = ShowWindow(window, SW_MINIMIZE);
            },
            "window-maximize" => unsafe {
                let maximizing = !IsZoomed(window).as_bool();
                let _ = ShowWindow(window, if maximizing { SW_MAXIMIZE } else { SW_RESTORE });
                self.restore_maximized.set(maximizing);
                self.persist_window_bounds(window);
            },
            "window-drag" => begin_window_move(window, None)?,
            "window-resize" => begin_window_move(
                window,
                Some(required(command.edge.as_deref(), "设置窗口缺少缩放方向")?),
            )?,
            "window-close" => {
                unsafe { PostMessageW(Some(window), WM_CLOSE, WPARAM(0), LPARAM(0)) }
                    .map_err(win_error)?;
            }
            "refresh" => self.enqueue(SettingsWindowCommand::Refresh),
            #[cfg(feature = "licensing")]
            "license-activate" => {
                let code = required(command.activation_code, "Missing activation code")?
                    .trim()
                    .to_ascii_lowercase();
                if clipferry_license_protocol::hex_decode::<32>(&code).is_err() {
                    return Err(io::Error::other(
                        "激活码应为 64 位字母和数字 / Invalid activation code",
                    ));
                }
                self.enqueue(SettingsWindowCommand::LicenseActivate(ActivationCode(
                    zeroize::Zeroizing::new(code),
                )));
            }
            #[cfg(feature = "licensing")]
            "license-refresh" => self.enqueue(SettingsWindowCommand::LicenseRefresh),
            #[cfg(feature = "licensing")]
            "license-release" => self.enqueue(SettingsWindowCommand::LicenseRelease),
            "ssh-refresh" => self.enqueue(SettingsWindowCommand::Ssh(crate::ssh::Command::Refresh)),
            "ssh-setup" => self.enqueue(SettingsWindowCommand::Ssh(crate::ssh::Command::Setup)),
            "ssh-setup-client" => {
                self.enqueue(SettingsWindowCommand::Ssh(crate::ssh::Command::SetupClient));
            }
            "ssh-request" => self.enqueue(SettingsWindowCommand::Ssh(
                crate::ssh::Command::Request(required(command.fingerprint, "Missing peer")?),
            )),
            "ssh-approve" => {
                self.enqueue(SettingsWindowCommand::Ssh(crate::ssh::Command::Approve {
                    id: required(command.id, "Missing request")?,
                    sid: required(command.sid, "Missing account")?,
                }));
            }
            "ssh-reject" => self.enqueue(SettingsWindowCommand::Ssh(crate::ssh::Command::Reject(
                required(command.id, "Missing request")?,
            ))),
            "ssh-cancel" => self.enqueue(SettingsWindowCommand::Ssh(crate::ssh::Command::Cancel(
                required(command.id, "Missing request")?,
            ))),
            "ssh-revoke" => self.enqueue(SettingsWindowCommand::Ssh(crate::ssh::Command::Revoke(
                required(command.id, "Missing grant")?,
            ))),
            "ssh-test" => self.enqueue(SettingsWindowCommand::Ssh(crate::ssh::Command::Test(
                required(command.id, "Missing connection")?,
            ))),
            "ssh-trust" => {
                self.enqueue(SettingsWindowCommand::Ssh(crate::ssh::Command::TrustHost {
                    id: required(command.id, "Missing connection")?,
                    expected: required(command.expected, "Missing reviewed host identity")?,
                }));
            }
            "ssh-terminal" => self.enqueue(SettingsWindowCommand::Ssh(
                crate::ssh::Command::Terminal(required(command.id, "Missing connection")?),
            )),
            "ssh-disconnect" => self.enqueue(SettingsWindowCommand::Ssh(
                crate::ssh::Command::Disconnect(required(command.id, "Missing SSH session")?),
            )),
            "pair" => self.enqueue(SettingsWindowCommand::Pair(required(
                command.fingerprint,
                "设置窗口缺少设备指纹",
            )?)),
            "approve-pairing" => self.enqueue(SettingsWindowCommand::ApprovePairing(true)),
            "reject-pairing" => self.enqueue(SettingsWindowCommand::ApprovePairing(false)),
            "revoke" => self.enqueue(SettingsWindowCommand::Revoke(required(
                command.fingerprint,
                "设置窗口缺少设备指纹",
            )?)),
            "select-peer" => self.enqueue(SettingsWindowCommand::SelectPeer(required(
                command.fingerprint,
                "设置窗口缺少设备指纹",
            )?)),
            "set-layout" => self.enqueue(SettingsWindowCommand::SetLayout {
                fingerprint: required(command.fingerprint, "设置窗口缺少布局设备")?,
                placement: required(command.placement, "设置窗口缺少屏幕布局")?,
            }),
            "save-boundary-map" => self.enqueue(SettingsWindowCommand::SaveBoundaryMap(required(
                command.boundary_map,
                "设置窗口缺少边界地图",
            )?)),
            "save-boundary-graph" => save_boundary_graph(
                &self.boundary_graph_path,
                required(command.graph_positions, "设置窗口缺少连接关系图坐标")?,
            )?,
            "save-boundary-workspace-v3" => {
                self.enqueue(SettingsWindowCommand::SaveBoundaryWorkspaceV3 {
                    expected_storage_revision: required(
                        command.expected_storage_revision,
                        "设置窗口缺少边界工作区修订号",
                    )?,
                    layout: required(command.boundary_layout, "设置窗口缺少逻辑边界草稿")?,
                    surface: required(command.surface, "设置窗口缺少边界编辑页面")?,
                });
            }
            "preview-boundary-workspace-v3" => {
                self.enqueue(SettingsWindowCommand::PreviewBoundaryWorkspaceV3 {
                    layout: required(command.boundary_layout, "设置窗口缺少实时布局草稿")?,
                });
            }
            "accept-boundary-workspace-preview" => {
                self.enqueue(SettingsWindowCommand::AcceptBoundaryWorkspacePreview {
                    sequence: required(command.sequence, "设置窗口缺少远端布局序号")?,
                });
            }
            "apply-boundary-workspace-v3" => {
                self.enqueue(SettingsWindowCommand::ApplyBoundaryWorkspaceV3 {
                    expected_storage_revision: required(
                        command.expected_storage_revision,
                        "设置窗口缺少边界工作区修订号",
                    )?,
                });
            }
            "save-connection" => self.enqueue(SettingsWindowCommand::SaveConnection {
                active_peer: required(command.active_peer, "设置窗口缺少活动设备")?,
                local_endpoint: required(command.local_endpoint, "设置窗口缺少本机地址")?,
                peer_endpoint: required(command.peer_endpoint, "设置窗口缺少对端地址")?,
                auto_receive: required(command.auto_receive, "设置窗口缺少自动接收状态")?,
            }),
            "set-cross-enabled" => self.enqueue(SettingsWindowCommand::SetCrossScreenEnabled(
                required(command.enabled, "设置窗口缺少跨屏开关状态")?,
            )),
            "set-pair-enabled" => self.enqueue(SettingsWindowCommand::SetPairEnabled {
                fingerprint: required(command.fingerprint, "设置窗口缺少设备指纹")?,
                enabled: required(command.enabled, "设置窗口缺少跨屏开关状态")?,
            }),
            "set-boundary-map-v2-enabled" => {
                self.enqueue(SettingsWindowCommand::SetBoundaryMapV2Enabled(required(
                    command.enabled,
                    "设置窗口缺少新版边界开关状态",
                )?));
            }
            "set-placement" => self.enqueue(SettingsWindowCommand::SetPlacement(required(
                command.placement,
                "设置窗口缺少屏幕布局",
            )?)),
            "set-corner-enabled" => self.enqueue(SettingsWindowCommand::SetCornerGuardEnabled(
                required(command.enabled, "设置窗口缺少四角保护状态")?,
            )),
            "set-corner-pixels" => self.enqueue(SettingsWindowCommand::SetCornerGuardPixels(
                required(command.pixels, "设置窗口缺少四角保护范围")?,
            )),
            "set-second-push" => self.enqueue(SettingsWindowCommand::SetSecondPush(required(
                command.enabled,
                "设置窗口缺少二次顶边状态",
            )?)),
            "set-fullscreen-pause" => self.enqueue(SettingsWindowCommand::SetFullscreenPause(
                required(command.enabled, "设置窗口缺少全屏暂停状态")?,
            )),
            "set-keep-link-warm" => self.enqueue(SettingsWindowCommand::SetKeepLinkWarm(required(
                command.enabled,
                "设置窗口缺少链路热身状态",
            )?)),
            "pause-cross" => self.enqueue(SettingsWindowCommand::PauseCrossScreen),
            "resume-cross" => self.enqueue(SettingsWindowCommand::ResumeCrossScreen),
            "return-local" => self.enqueue(SettingsWindowCommand::ReturnLocal),
            "restart-admin" => self.enqueue(SettingsWindowCommand::RestartElevated),
            "compatibility" => self.enqueue(SettingsWindowCommand::ShowCompatibility),
            "set-shared-enabled" => self.enqueue(SettingsWindowCommand::SetSharedClipboardEnabled(
                required(command.enabled, "设置窗口缺少共享剪贴板状态")?,
            )),
            "open-transfer" => self.enqueue(SettingsWindowCommand::OpenTransfer {
                history: command.history.unwrap_or(false),
            }),
            "set-auto-popup" => self.enqueue(SettingsWindowCommand::SetTransferAutoPopup(
                required(command.enabled, "设置窗口缺少自动弹出状态")?,
            )),
            "accept-pending" => self.enqueue(SettingsWindowCommand::AcceptPending),
            "pause-all" => self.enqueue(SettingsWindowCommand::PauseAll),
            "resume-all" => self.enqueue(SettingsWindowCommand::ResumeAll),
            "cancel-all" => self.enqueue(SettingsWindowCommand::CancelAll),
            "pause-task" => self.enqueue(SettingsWindowCommand::PauseTask(required(
                command.task,
                "设置窗口缺少传输任务标识",
            )?)),
            "cancel-task" => self.enqueue(SettingsWindowCommand::CancelTask(required(
                command.task,
                "设置窗口缺少传输任务标识",
            )?)),
            "set-history-limit" => self.enqueue(SettingsWindowCommand::SetHistoryLimit(
                usize::try_from(required(command.value, "设置窗口缺少历史数量")?)
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "历史数量超出范围"))?,
            )),
            "set-row-height" => self.enqueue(SettingsWindowCommand::SetHistoryRowHeight(
                u16::try_from(required(command.value, "设置窗口缺少历史行高")?)
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "历史行高超出范围"))?,
            )),
            "set-autostart" => self.enqueue(SettingsWindowCommand::SetAutostart(required(
                command.enabled,
                "设置窗口缺少自启动状态",
            )?)),
            "set-language" => self.enqueue(SettingsWindowCommand::SetLanguage(required(
                command.language,
                "设置窗口缺少语言",
            )?)),
            "set-theme" => self.enqueue(SettingsWindowCommand::SetTheme(required(
                command.theme,
                "设置窗口缺少主题",
            )?)),
            "set-font-scale" => self.enqueue(SettingsWindowCommand::SetFontScale(
                u16::try_from(required(command.value, "设置窗口缺少字体缩放")?)
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "字体缩放超出范围"))?,
            )),
            "open-log" => self.enqueue(SettingsWindowCommand::OpenLog),
            "screenshot" => self.enqueue(SettingsWindowCommand::Screenshot),
            "set-screenshot-shortcut" => {
                self.enqueue(SettingsWindowCommand::SetScreenshotShortcut(required(
                    command.shortcut,
                    "Missing screenshot shortcut",
                )?));
            }
            "open-log-folder" => self.enqueue(SettingsWindowCommand::OpenLogFolder),
            "open-source" => self.enqueue(SettingsWindowCommand::OpenSource),
            "exit" => self.enqueue(SettingsWindowCommand::Exit),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "设置窗口命令不受支持",
                ));
            }
        }
        Ok(())
    }

    fn hide(&self, window: HWND) {
        self.persist_window_bounds(window);
        self.hidden.set(true);
        self.close_webview();
        unsafe {
            let _ = ShowWindow(window, SW_HIDE);
        }
    }

    fn persist_window_bounds(&self, window: HWND) {
        if let Ok(bounds) = current_window_bounds(window) {
            self.restore_maximized.set(bounds.maximized);
            let _ = save_window_bounds(&self.window_bounds_path, bounds);
        }
    }
}

fn required<T>(value: Option<T>, message: &'static str) -> io::Result<T> {
    value.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, message))
}

fn validate_page(page: &str) -> &str {
    match page {
        "status" | "devices" | "cross" | "clipboard" | "transfer" | "connection" | "general"
        | "support" | "license" | "about" | "remote" => page,
        _ => "status",
    }
}

fn default_window_bounds() -> SavedWindowBounds {
    let screen_width = unsafe { GetSystemMetrics(SM_CXSCREEN) }.max(WINDOW_MIN_WIDTH);
    let screen_height = unsafe { GetSystemMetrics(SM_CYSCREEN) }.max(WINDOW_MIN_HEIGHT);
    let width = WINDOW_WIDTH
        .min(screen_width.saturating_sub(48))
        .max(WINDOW_MIN_WIDTH);
    let height = WINDOW_HEIGHT
        .min(screen_height.saturating_sub(48))
        .max(WINDOW_MIN_HEIGHT);
    SavedWindowBounds {
        x: (screen_width - width) / 2,
        y: (screen_height - height) / 2,
        width,
        height,
        maximized: false,
    }
}

fn clamp_window_bounds(mut bounds: SavedWindowBounds) -> SavedWindowBounds {
    let virtual_x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let virtual_y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let virtual_width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) }.max(WINDOW_MIN_WIDTH);
    let virtual_height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) }.max(WINDOW_MIN_HEIGHT);
    bounds.width = bounds.width.clamp(WINDOW_MIN_WIDTH, virtual_width);
    bounds.height = bounds.height.clamp(WINDOW_MIN_HEIGHT, virtual_height);
    let visible = 72;
    bounds.x = bounds.x.clamp(
        virtual_x - bounds.width + visible,
        virtual_x + virtual_width - visible,
    );
    bounds.y = bounds
        .y
        .clamp(virtual_y, virtual_y + virtual_height - visible);
    bounds
}

fn current_window_bounds(window: HWND) -> io::Result<SavedWindowBounds> {
    let maximized = unsafe { IsZoomed(window) }.as_bool();
    let mut placement = WINDOWPLACEMENT {
        length: u32::try_from(std::mem::size_of::<WINDOWPLACEMENT>()).unwrap_or(u32::MAX),
        ..Default::default()
    };
    unsafe { GetWindowPlacement(window, &raw mut placement) }.map_err(win_error)?;
    // `rcNormalPosition` can lag behind a live frameless popup until the thread processes
    // another placement message. Read the actual rectangle while restored; retain the saved
    // normal rectangle only while maximized so restoring never persists the monitor bounds.
    let rect = if maximized {
        placement.rcNormalPosition
    } else {
        let mut rect = RECT::default();
        unsafe { GetWindowRect(window, &raw mut rect) }.map_err(win_error)?;
        rect
    };
    Ok(clamp_window_bounds(SavedWindowBounds {
        x: rect.left,
        y: rect.top,
        width: rect.right - rect.left,
        height: rect.bottom - rect.top,
        maximized,
    }))
}

fn save_window_bounds(path: &Path, bounds: SavedWindowBounds) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        path,
        format!(
            "{WINDOW_BOUNDS_HEADER}\nx={}\ny={}\nwidth={}\nheight={}\nmaximized={}\n",
            bounds.x,
            bounds.y,
            bounds.width,
            bounds.height,
            u8::from(bounds.maximized)
        ),
    )
}

fn validate_boundary_graph(positions: &BTreeMap<String, BoundaryGraphPoint>) -> io::Result<()> {
    if positions.len() > MAX_BOUNDARY_GRAPH_NODES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "连接关系图设备数量超出限制",
        ));
    }
    for (fingerprint, point) in positions {
        if fingerprint.is_empty()
            || fingerprint.len() > 96
            || !point.x.is_finite()
            || !point.y.is_finite()
            || !(0.0..=1_000.0).contains(&point.x)
            || !(0.0..=1_000.0).contains(&point.y)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "连接关系图坐标无效",
            ));
        }
    }
    Ok(())
}

fn save_boundary_graph(
    path: &Path,
    positions: BTreeMap<String, BoundaryGraphPoint>,
) -> io::Result<()> {
    validate_boundary_graph(&positions)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec(&SavedBoundaryGraph {
        schema_version: 1,
        positions,
    })
    .map_err(io::Error::other)?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    file.write_all(&bytes)?;
    file.sync_all()
}

fn load_boundary_graph(path: &Path) -> io::Result<SavedBoundaryGraph> {
    let mut file = fs::File::open(path)?;
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(BOUNDARY_GRAPH_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > BOUNDARY_GRAPH_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "连接关系图配置过大",
        ));
    }
    let graph: SavedBoundaryGraph = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if graph.schema_version != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "连接关系图配置版本无效",
        ));
    }
    validate_boundary_graph(&graph.positions)?;
    Ok(graph)
}

fn load_window_bounds(path: &Path) -> io::Result<SavedWindowBounds> {
    let text = fs::read_to_string(path)?;
    let mut lines = text.lines();
    if lines.next() != Some(WINDOW_BOUNDS_HEADER) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "窗口状态版本无效",
        ));
    }
    let parse = |line: Option<&str>, name: &str| -> io::Result<i32> {
        line.and_then(|value| value.strip_prefix(&format!("{name}=")))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "窗口状态不完整"))?
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "窗口状态数字无效"))
    };
    let x = parse(lines.next(), "x")?;
    let y = parse(lines.next(), "y")?;
    let width = parse(lines.next(), "width")?;
    let height = parse(lines.next(), "height")?;
    let maximized = match lines.next() {
        Some("maximized=0") => false,
        Some("maximized=1") => true,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "窗口最大化状态无效",
            ));
        }
    };
    if lines.any(|line| !line.is_empty()) || width <= 0 || height <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "窗口状态包含无效字段",
        ));
    }
    Ok(SavedWindowBounds {
        x,
        y,
        width,
        height,
        maximized,
    })
}

fn begin_window_move(window: HWND, edge: Option<&str>) -> io::Result<()> {
    let hit = match edge {
        None => 2,
        Some("left") => 10,
        Some("right") => 11,
        Some("top") => 12,
        Some("top-left") => 13,
        Some("top-right") => 14,
        Some("bottom") => 15,
        Some("bottom-left") => 16,
        Some("bottom-right") => 17,
        Some(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "未知的缩放方向",
            ));
        }
    };
    unsafe {
        let _ = ReleaseCapture();
        let _ = SendMessageW(window, WM_NCLBUTTONDOWN, Some(WPARAM(hit)), Some(LPARAM(0)));
    }
    Ok(())
}

fn webview_bounds(window: HWND) -> io::Result<RECT> {
    let _dpi = crate::cross_screen::ThreadDpiAwarenessGuard::enter_per_monitor()?;
    let mut bounds = RECT::default();
    unsafe { GetClientRect(window, &raw mut bounds) }.map_err(win_error)?;
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

#[allow(clippy::too_many_lines)]
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
                    if !pointer.is_null()
                        && let Err(error) = (&*pointer).handle_web_message(window, &message)
                    {
                        let payload = serde_json::json!({
                            "type": "toast",
                            "message": format!("操作未完成：{error}")
                        })
                        .to_string();
                        let _ = (&*pointer).post_json(&payload);
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
            WM_CLOSE => {
                state.hide(window);
                return LRESULT(0);
            }
            WM_SIZE => {
                state.resize_webview(window);
                return LRESULT(0);
            }
            WM_WINDOWPOSCHANGED => state.resize_webview(window),
            WM_EXITSIZEMOVE => {
                state.persist_window_bounds(window);
                return LRESULT(0);
            }
            WM_GETMINMAXINFO => {
                let info = lparam.0 as *mut MINMAXINFO;
                if !info.is_null() {
                    unsafe {
                        (*info).ptMinTrackSize.x = WINDOW_MIN_WIDTH;
                        (*info).ptMinTrackSize.y = WINDOW_MIN_HEIGHT;
                    }
                    apply_maximized_work_area(window, info);
                    return LRESULT(0);
                }
            }
            WM_NCCALCSIZE => return LRESULT(0),
            WM_NCHITTEST => return hit_test(window, lparam),
            WM_NCDESTROY => {
                state.close_webview();
                unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, 0) };
            }
            _ => {}
        }
    }
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

fn apply_maximized_work_area(window: HWND, info: *mut MINMAXINFO) {
    if info.is_null() {
        return;
    }
    let monitor = unsafe { MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST) };
    let mut monitor_info = MONITORINFO {
        cbSize: u32::try_from(std::mem::size_of::<MONITORINFO>()).unwrap_or(u32::MAX),
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(monitor, &raw mut monitor_info) }.as_bool() {
        return;
    }
    let (position_x, position_y, width, height) =
        maximized_work_area(monitor_info.rcMonitor, monitor_info.rcWork);
    unsafe {
        (*info).ptMaxPosition.x = position_x;
        (*info).ptMaxPosition.y = position_y;
        (*info).ptMaxSize.x = width;
        (*info).ptMaxSize.y = height;
        (*info).ptMaxTrackSize.x = width;
        (*info).ptMaxTrackSize.y = height;
    }
}

fn maximized_work_area(monitor: RECT, work: RECT) -> (i32, i32, i32, i32) {
    (
        work.left - monitor.left,
        work.top - monitor.top,
        (work.right - work.left).max(1),
        (work.bottom - work.top).max(1),
    )
}

fn hit_test(window: HWND, lparam: LPARAM) -> LRESULT {
    const HTCLIENT: isize = 1;
    const HTCAPTION: isize = 2;
    const HTLEFT: isize = 10;
    const HTRIGHT: isize = 11;
    const HTTOP: isize = 12;
    const HTTOPLEFT: isize = 13;
    const HTTOPRIGHT: isize = 14;
    const HTBOTTOM: isize = 15;
    const HTBOTTOMLEFT: isize = 16;
    const HTBOTTOMRIGHT: isize = 17;

    let packed = lparam.0.to_le_bytes();
    let x = i32::from(i16::from_le_bytes([packed[0], packed[1]]));
    let y = i32::from(i16::from_le_bytes([packed[2], packed[3]]));
    let mut rect = RECT::default();
    if unsafe { GetWindowRect(window, &raw mut rect) }.is_err() {
        return LRESULT(HTCLIENT);
    }
    let local_x = x - rect.left;
    let local_y = y - rect.top;
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    if !unsafe { IsZoomed(window) }.as_bool() {
        let left = local_x < RESIZE_BORDER;
        let right = local_x >= width - RESIZE_BORDER;
        let top = local_y < RESIZE_BORDER;
        let bottom = local_y >= height - RESIZE_BORDER;
        return match (left, right, top, bottom) {
            (true, _, true, _) => LRESULT(HTTOPLEFT),
            (_, true, true, _) => LRESULT(HTTOPRIGHT),
            (true, _, _, true) => LRESULT(HTBOTTOMLEFT),
            (_, true, _, true) => LRESULT(HTBOTTOMRIGHT),
            (true, _, _, _) => LRESULT(HTLEFT),
            (_, true, _, _) => LRESULT(HTRIGHT),
            (_, _, true, _) => LRESULT(HTTOP),
            (_, _, _, true) => LRESULT(HTBOTTOM),
            _ => {
                if local_y < TITLEBAR_HEIGHT && local_x < width - TITLEBAR_BUTTONS_WIDTH {
                    LRESULT(HTCAPTION)
                } else {
                    LRESULT(HTCLIENT)
                }
            }
        };
    }
    if local_y < TITLEBAR_HEIGHT && local_x < width - TITLEBAR_BUTTONS_WIDTH {
        LRESULT(HTCAPTION)
    } else {
        LRESULT(HTCLIENT)
    }
}

fn encode_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        output.push(char::from(TABLE[usize::from(first >> 2)]));
        output.push(char::from(
            TABLE[usize::from((first & 0x03) << 4 | second >> 4)],
        ));
        if chunk.len() > 1 {
            output.push(char::from(
                TABLE[usize::from((second & 0x0f) << 2 | third >> 6)],
            ));
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(char::from(TABLE[usize::from(third & 0x3f)]));
        } else {
            output.push('=');
        }
    }
    output
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[allow(clippy::needless_pass_by_value)]
fn win_error(error: windows_core::Error) -> io::Error {
    io::Error::other(error.to_string())
}

#[allow(clippy::needless_pass_by_value)]
fn webview_error(error: webview2_com::Error) -> io::Error {
    io::Error::other(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_WINDOW_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn test_state() -> ProcedureState {
        ProcedureState {
            target: HWND::default(),
            commands: SettingsWindowCommands { dispatch: 1 },
            hidden: Cell::new(true),
            webview: RefCell::new(None),
            webview_ready: Cell::new(false),
            webview_user_data: PathBuf::new(),
            window_bounds_path: PathBuf::new(),
            boundary_graph_path: PathBuf::new(),
            restore_maximized: Cell::new(false),
            html: String::new(),
            model_json: RefCell::new(String::new()),
            page: RefCell::new("status".to_owned()),
            pending: RefCell::new(VecDeque::new()),
        }
    }

    #[test]
    fn base64_encoder_matches_known_vectors() {
        assert_eq!(encode_base64(b""), "");
        assert_eq!(encode_base64(b"f"), "Zg==");
        assert_eq!(encode_base64(b"fo"), "Zm8=");
        assert_eq!(encode_base64(b"foo"), "Zm9v");
    }

    #[test]
    fn settings_pages_are_bounded() {
        assert_eq!(validate_page("devices"), "devices");
        assert_eq!(validate_page("license"), "license");
        assert_eq!(validate_page("not-a-page"), "status");
    }

    #[test]
    fn embedded_interface_contains_all_graphical_pages() {
        for page in [
            "status",
            "devices",
            "cross",
            "clipboard",
            "transfer",
            "remote",
            "connection",
            "general",
            "license",
            "about",
        ] {
            assert!(
                WEB_CONTENT.contains(&format!("['{page}'")),
                "missing {page}"
            );
        }
        assert!(WEB_CONTENT.contains("data-action=\"approve-pairing\""));
        assert!(WEB_CONTENT.contains("modal.type === 'compatibility'"));
        assert!(WEB_CONTENT.contains("recordStatusTelemetry"));
        assert!(WEB_CONTENT.contains("class=\"latency-chart\""));
        assert!(WEB_CONTENT.contains("activityTelemetryMarkup"));
    }

    #[test]
    fn web_commands_preserve_connection_and_pairing_values() {
        let state = test_state();
        state
            .handle_web_message(
                HWND::default(),
                r#"{"action":"save-connection","activePeer":"AA","localEndpoint":"127.0.0.1:1","peerEndpoint":"127.0.0.1:2","autoReceive":true}"#,
            )
            .unwrap();
        state
            .handle_web_message(HWND::default(), r#"{"action":"approve-pairing"}"#)
            .unwrap();
        state
            .handle_web_message(
                HWND::default(),
                r#"{"action":"set-layout","fingerprint":"BB","placement":"left"}"#,
            )
            .unwrap();
        state
            .handle_web_message(
                HWND::default(),
                r#"{"action":"set-language","language":"en"}"#,
            )
            .unwrap();
        state
            .handle_web_message(HWND::default(), r#"{"action":"set-theme","theme":"dark"}"#)
            .unwrap();
        state
            .handle_web_message(
                HWND::default(),
                r#"{"action":"set-font-scale","value":115}"#,
            )
            .unwrap();
        state
            .handle_web_message(
                HWND::default(),
                r#"{"action":"set-boundary-map-v2-enabled","enabled":false}"#,
            )
            .unwrap();
        let commands = state.pending.borrow_mut().drain(..).collect::<Vec<_>>();
        assert_eq!(
            commands,
            vec![
                SettingsWindowCommand::SaveConnection {
                    active_peer: "AA".to_owned(),
                    local_endpoint: "127.0.0.1:1".to_owned(),
                    peer_endpoint: "127.0.0.1:2".to_owned(),
                    auto_receive: true,
                },
                SettingsWindowCommand::ApprovePairing(true),
                SettingsWindowCommand::SetLayout {
                    fingerprint: "BB".to_owned(),
                    placement: "left".to_owned(),
                },
                SettingsWindowCommand::SetLanguage("en".to_owned()),
                SettingsWindowCommand::SetTheme("dark".to_owned()),
                SettingsWindowCommand::SetFontScale(115),
                SettingsWindowCommand::SetBoundaryMapV2Enabled(false),
            ]
        );
    }

    #[test]
    fn web_command_preserves_complete_boundary_map_value() {
        let state = test_state();
        state
            .handle_web_message(
                HWND::default(),
                r#"{"action":"save-boundary-map","boundaryMap":{"schemaVersion":2,"revision":7,"segments":[{"id":"top-b","peerFingerprint":"AA:BB","edge":"top","start":1024,"end":8192}]}}"#,
            )
            .unwrap();
        assert_eq!(
            state.pending.borrow_mut().pop_front(),
            Some(SettingsWindowCommand::SaveBoundaryMap(serde_json::json!({
                "schemaVersion": 2,
                "revision": 7,
                "segments": [{
                    "id": "top-b",
                    "peerFingerprint": "AA:BB",
                    "edge": "top",
                    "start": 1024,
                    "end": 8192,
                }],
            })))
        );
    }

    #[test]
    fn web_commands_preserve_v3_boundary_workspace_revisions_and_layout() {
        let state = test_state();
        state
            .handle_web_message(
                HWND::default(),
                r#"{"action":"save-boundary-workspace-v3","expectedStorageRevision":4,"surface":"boundary","boundaryLayout":{"schemaVersion":3,"revision":8,"localFingerprint":"AA:BB","screens":[],"boundaries":[]}}"#,
            )
            .unwrap();
        state
            .handle_web_message(
                HWND::default(),
                r#"{"action":"apply-boundary-workspace-v3","expectedStorageRevision":5}"#,
            )
            .unwrap();
        assert_eq!(
            state.pending.borrow_mut().drain(..).collect::<Vec<_>>(),
            vec![
                SettingsWindowCommand::SaveBoundaryWorkspaceV3 {
                    expected_storage_revision: 4,
                    layout: serde_json::json!({
                        "schemaVersion": 3,
                        "revision": 8,
                        "localFingerprint": "AA:BB",
                        "screens": [],
                        "boundaries": [],
                    }),
                    surface: "boundary".to_owned(),
                },
                SettingsWindowCommand::ApplyBoundaryWorkspaceV3 {
                    expected_storage_revision: 5,
                },
            ]
        );
    }

    #[test]
    fn web_commands_preserve_live_boundary_preview_and_accept_sequence() {
        let state = test_state();
        state
            .handle_web_message(
                HWND::default(),
                r#"{"action":"preview-boundary-workspace-v3","boundaryLayout":{"schemaVersion":3,"revision":8,"localFingerprint":"AA:BB","screens":[],"boundaries":[]}}"#,
            )
            .unwrap();
        state
            .handle_web_message(
                HWND::default(),
                r#"{"action":"accept-boundary-workspace-preview","sequence":27}"#,
            )
            .unwrap();
        assert_eq!(
            state.pending.borrow_mut().drain(..).collect::<Vec<_>>(),
            vec![
                SettingsWindowCommand::PreviewBoundaryWorkspaceV3 {
                    layout: serde_json::json!({
                        "schemaVersion": 3,
                        "revision": 8,
                        "localFingerprint": "AA:BB",
                        "screens": [],
                        "boundaries": [],
                    }),
                },
                SettingsWindowCommand::AcceptBoundaryWorkspacePreview { sequence: 27 },
            ]
        );
    }

    #[test]
    fn ssh_disconnect_command_preserves_the_observed_session_id() {
        let state = test_state();
        state
            .handle_web_message(
                HWND::default(),
                r#"{"action":"ssh-disconnect","id":"0123456789abcdef0123456789abcdef"}"#,
            )
            .unwrap();
        assert_eq!(
            state.pending.borrow_mut().pop_front(),
            Some(SettingsWindowCommand::Ssh(crate::ssh::Command::Disconnect(
                "0123456789abcdef0123456789abcdef".to_owned()
            )))
        );
    }

    #[test]
    fn window_bounds_round_trip_is_strict() {
        let path = std::env::temp_dir().join(format!(
            "pierdesk-settings-bounds-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let bounds = SavedWindowBounds {
            x: 120,
            y: 80,
            width: 1280,
            height: 820,
            maximized: true,
        };
        save_window_bounds(&path, bounds).unwrap();
        assert_eq!(load_window_bounds(&path).unwrap(), bounds);
        std::fs::write(&path, "ClipFerrySettingsWindowV1\nx=0\n").unwrap();
        assert!(load_window_bounds(&path).is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn boundary_graph_positions_round_trip_in_the_application_data_file() {
        let root = std::env::temp_dir().join(format!(
            "pierdesk-boundary-graph-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = root.join(BOUNDARY_GRAPH_FILE);
        let positions = BTreeMap::from([
            (
                "AA:BB".to_owned(),
                BoundaryGraphPoint { x: 320.0, y: 180.0 },
            ),
            (
                "CC:DD".to_owned(),
                BoundaryGraphPoint { x: 610.0, y: 205.0 },
            ),
        ]);
        save_boundary_graph(&path, positions.clone()).unwrap();
        assert_eq!(load_boundary_graph(&path).unwrap().positions, positions);
        assert!(root.starts_with(std::env::temp_dir()));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn maximized_bounds_keep_each_taskbar_edge_visible() {
        let monitor = RECT {
            left: -1920,
            top: 0,
            right: 0,
            bottom: 1080,
        };
        assert_eq!(
            maximized_work_area(
                monitor,
                RECT {
                    left: -1920,
                    top: 0,
                    right: 0,
                    bottom: 1040,
                }
            ),
            (0, 0, 1920, 1040)
        );
        assert_eq!(
            maximized_work_area(
                monitor,
                RECT {
                    left: -1880,
                    top: 0,
                    right: 0,
                    bottom: 1080,
                }
            ),
            (40, 0, 1880, 1080)
        );
        assert_eq!(
            maximized_work_area(
                monitor,
                RECT {
                    left: -1920,
                    top: 44,
                    right: 0,
                    bottom: 1080,
                }
            ),
            (0, 44, 1920, 1036)
        );
    }

    #[test]
    fn native_maximized_window_stays_inside_the_monitor_work_area() {
        let _guard = TEST_WINDOW_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = std::env::temp_dir().join(format!(
            "pierdesk-settings-maximize-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let window = SettingsWindow::create(
            HWND::default(),
            HICON::default(),
            SettingsWindowCommands { dispatch: 1001 },
            &root,
        )
        .unwrap();
        unsafe {
            let _ = ShowWindow(window.handle, SW_MAXIMIZE);
        }
        let monitor = unsafe { MonitorFromWindow(window.handle, MONITOR_DEFAULTTONEAREST) };
        let mut monitor_info = MONITORINFO {
            cbSize: u32::try_from(std::mem::size_of::<MONITORINFO>()).unwrap_or(u32::MAX),
            ..Default::default()
        };
        assert!(unsafe { GetMonitorInfoW(monitor, &raw mut monitor_info) }.as_bool());
        let mut rect = RECT::default();
        unsafe { GetWindowRect(window.handle, &raw mut rect) }.unwrap();
        assert_eq!(rect, monitor_info.rcWork);
        drop(window);
        assert!(root.starts_with(std::env::temp_dir()));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn native_move_and_resize_event_persists_window_bounds() {
        let _guard = TEST_WINDOW_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = std::env::temp_dir().join(format!(
            "pierdesk-settings-geometry-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let expected = clamp_window_bounds(SavedWindowBounds {
            x: unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) } + 160,
            y: unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) } + 112,
            width: 1000,
            height: 700,
            maximized: false,
        });
        let window = SettingsWindow::create(
            HWND::default(),
            HICON::default(),
            SettingsWindowCommands { dispatch: 1001 },
            &root,
        )
        .unwrap();
        unsafe {
            let _ = ShowWindow(window.handle, SW_RESTORE);
            windows::Win32::UI::WindowsAndMessaging::MoveWindow(
                window.handle,
                expected.x,
                expected.y,
                expected.width,
                expected.height,
                true,
            )
        }
        .unwrap();
        let live = current_window_bounds(window.handle).unwrap();
        assert_ne!(live, default_window_bounds());
        unsafe {
            SendMessageW(
                window.handle,
                WM_EXITSIZEMOVE,
                Some(WPARAM(0)),
                Some(LPARAM(0)),
            );
        }
        let saved = load_window_bounds(&root.join(WINDOW_BOUNDS_FILE)).unwrap();
        assert_ne!(saved, default_window_bounds());
        drop(window);
        let restored = SettingsWindow::create(
            HWND::default(),
            HICON::default(),
            SettingsWindowCommands { dispatch: 1001 },
            &root,
        )
        .unwrap();
        assert_eq!(current_window_bounds(restored.handle).unwrap(), live);
        drop(restored);
        assert!(root.starts_with(std::env::temp_dir()));
        let _ = std::fs::remove_dir_all(root);
    }

    fn evaluate_webview(window: &SettingsWindow, script: &str) -> serde_json::Value {
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

    fn assert_boundary_save_receipt_unlocks_editor(window: &SettingsWindow) {
        let result = evaluate_webview(
            window,
            "const editor=boundaryV3State();editor.saving=true;editor.dirty=true;window.dispatchEvent(new MessageEvent('message',{data:{type:'boundary-save-result',success:true,message:'人工边界草稿已保存'}}));[editor.saving,editor.dirty]",
        );
        assert_eq!(result, serde_json::json!([false, false]));

        let failed = evaluate_webview(
            window,
            "{const editor=boundaryV3State();editor.saving=true;editor.dirty=true;window.dispatchEvent(new MessageEvent('message',{data:{type:'boundary-save-result',success:false,message:'修订冲突'}}));[editor.saving,editor.dirty,editor.message]}",
        );
        assert_eq!(
            failed,
            serde_json::json!([false, false, "保存失败，已恢复后台中的上一份草稿。"])
        );
    }

    fn assert_boundary_revision_normalization(window: &SettingsWindow) {
        let result = evaluate_webview(
            window,
            r"{
              const savedModel=boundaryClone(state.model);
              const local='AA:BB',peer='CC:DD';
              const screen=(id)=>({deviceFingerprint:id,pixelWidth:1920,pixelHeight:1080,physicalWidthMillimeters:500,physicalHeightMillimeters:300,rotationQuarterTurns:0,canvasXMillimeters:0,canvasYMillimeters:0});
              const boundary={id:'b',peerFingerprint:peer,mappingRevision:3,localConfirmedRevision:3,peerConfirmedRevision:3,reversed:false,source:{segments:[{edge:'top',start:10,end:100}]},target:{segments:[{edge:'bottom',start:20,end:110}]}};
              state.model.boundaryWorkspaceV3={schemaVersion:3,storageRevision:7,draft:{schemaVersion:3,revision:9,localFingerprint:local,screens:[screen(local),screen(peer)],boundaries:[boundaryClone(boundary)]},applied:null};
              boundaryV3LoadModel(state.model,false,false);
              const editor=boundaryV3State();
              editor.draft.boundaries[0].mappingRevision=4;editor.draft.boundaries[0].localConfirmedRevision=0;editor.draft.boundaries[0].peerConfirmedRevision=0;
              boundaryV3NormalizeRevisionsAgainstModel();
              const noOp=[editor.draft.boundaries[0].mappingRevision,editor.draft.boundaries[0].localConfirmedRevision,editor.draft.boundaries[0].peerConfirmedRevision];
              state.model.boundaryWorkspaceV3.draft.boundaries[0].localConfirmedRevision=0;
              boundaryV3LoadModel(state.model,false,false);
              boundaryV3State().draft.boundaries[0].localConfirmedRevision=3;
              boundaryV3State().draft.boundaries[0].peerConfirmedRevision=0;
              boundaryV3NormalizeRevisionsAgainstModel();
              const confirmationOnly=[boundaryV3State().draft.boundaries[0].mappingRevision,boundaryV3State().draft.boundaries[0].localConfirmedRevision,boundaryV3State().draft.boundaries[0].peerConfirmedRevision];
              state.model.boundaryWorkspaceV3.draft.boundaries[0].localConfirmedRevision=3;
              boundaryV3LoadModel(state.model,false,false);
              const editorAfterConfirmation=boundaryV3State();
              editorAfterConfirmation.draft.boundaries[0].peerConfirmedRevision=0;
              editorAfterConfirmation.draft.boundaries[0].source.segments[0].start=11;editorAfterConfirmation.draft.boundaries[0].mappingRevision=99;
              boundaryV3NormalizeRevisionsAgainstModel();
              const changed=[editorAfterConfirmation.draft.boundaries[0].mappingRevision,editorAfterConfirmation.draft.boundaries[0].localConfirmedRevision,editorAfterConfirmation.draft.boundaries[0].peerConfirmedRevision];
              state.model.peers=[{fingerprint:peer,label:'Peer',online:true}];
              const waitingLocal=boundaryV3BoundaryList({boundaries:[{...boundary,localConfirmedRevision:0,peerConfirmedRevision:3}]}).includes('等待本机确认');
              const waitingPeer=boundaryV3BoundaryList({boundaries:[{...boundary,localConfirmedRevision:3,peerConfirmedRevision:0}]}).includes('等待对端确认');
              state.model=savedModel;state.boundaryV3=null;
              [noOp,confirmationOnly,changed,waitingLocal,waitingPeer]
            }",
        );
        assert_eq!(
            result,
            serde_json::json!([[3, 3, 3], [3, 3, 3], [4, 0, 0], true, true])
        );
    }

    fn assert_boundary_graph_persists_across_webview_recreation(
        window: &SettingsWindow,
        root: &Path,
    ) {
        assert_eq!(
            evaluate_webview(
                window,
                "boundaryV3SaveGraphPositions({'AA:BB':{x:321,y:187}});true",
            ),
            serde_json::json!(true)
        );
        let graph_path = root.join(BOUNDARY_GRAPH_FILE);
        for _ in 0..100 {
            if load_boundary_graph(&graph_path).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            load_boundary_graph(&graph_path).unwrap().positions["AA:BB"],
            BoundaryGraphPoint { x: 321.0, y: 187.0 }
        );
        window.state.hide(window.handle);
        window.show("cross").unwrap();
        let mut restored_graph = serde_json::Value::Null;
        for _ in 0..100 {
            restored_graph = evaluate_webview(
                window,
                "[boundaryV3State().graphStoredPositions['AA:BB']?.x,boundaryV3State().graphStoredPositions['AA:BB']?.y]",
            );
            if restored_graph[0].as_f64() == Some(321.0) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(restored_graph, serde_json::json!([321, 187]));
    }

    fn assert_english_dynamic_text_is_localized(window: &SettingsWindow) {
        assert_eq!(
            evaluate_webview(
                window,
                r"(()=>{const previous=currentModel().language;currentModel().language='en';const values=[
                  translateText('右侧 · 1 条边界'),
                  translateText('最低延迟 2.3 ms'),
                  translateText('跨屏控制运行中'),
                  translateText('暂未发现可配对的新设备。请确认另一台电脑已打开 Pierdesk 并位于同一局域网。'),
                  translateText('范围 1–500'),
                  translateText('条'),
                  translateText('24 个文件 · 2 个文件夹'),
                  translateText('DESKTOP-STUDIO → 本机'),
                  translateText('198 像素'),
                  translateText('文字 + HTML'),
                  sshErrorText('无法连接对方电脑：Device is offline or not discovered。请确认对方在线，且两台电脑均允许专用网络控制端口 45833。'),
                  sshErrorText('未知的 SSH 中文错误'),
                  sshIssue('本机 SSH 配置损坏'),
                  runtimeErrorText('连接对端超时，请检查防火墙'),
                  runtimeErrorText('后台服务已停止：Access denied'),
                  runtimeErrorText('后台服务已停止：内部线程异常'),
                  runtimeErrorText('未知的后台中文错误'),
                  translateText('请打开诊断日志'),
                  pairingMessageText('配对未完成：目标设备拒绝连接')
                ];currentModel().language=previous;return values})()",
            ),
            serde_json::json!([
                "Right · 1 boundary(ies)",
                "Lowest latency 2.3 ms",
                "Cross-screen control · Running",
                "No new devices are available to pair. Make sure Pierdesk is open on another PC on the same LAN.",
                "Range 1–500",
                "items",
                "24 file(s) · 2 folder(s)",
                "DESKTOP-STUDIO → Local device",
                "198 px",
                "Text + HTML",
                "Unable to reach the target; the cause is unconfirmed. Run Pierdesk on both computers. On the target, open Remote work > Local access to check or repair access.",
                "The SSH operation failed. Open the diagnostic log for the complete error.",
                "The local SSH configuration needs attention. Open the diagnostic log for the complete error.",
                "Cross-screen connection timed out. Make sure Pierdesk is running on the other computer, both computers can reach each other on the LAN, and private-network access is allowed in Windows Firewall.",
                "The background service stopped: Access denied",
                "The background service stopped. Open the diagnostic log for details.",
                "An internal operation failed. Open the diagnostic log for the complete error.",
                "Open the diagnostic log",
                "Pairing did not complete: An internal operation failed. Open the diagnostic log for the complete error."
            ])
        );
    }

#[cfg(feature = "licensing")]
    fn assert_license_page(window: &SettingsWindow) {
        window.show("license").unwrap();
        let mut license_page = serde_json::Value::Null;
        for _ in 0..100 {
            license_page = evaluate_webview(
                window,
                "[document.querySelector('#license-code')?.type,!!document.querySelector('[data-license=release]'),document.querySelectorAll('.license-steps li').length,document.body.textContent.includes('不提供试用'),!!document.querySelector('[data-license-details]')]",
            );
            if license_page[0].as_str() == Some("password") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            license_page,
            serde_json::json!(["password", false, 3, false, false])
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn actual_webview_renders_all_navigation_and_releases_on_close() {
        let _guard = TEST_WINDOW_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = std::env::temp_dir().join(format!(
            "pierdesk-settings-window-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let model = serde_json::json!({
            "type":"model", "licenseEnabled":cfg!(feature = "licensing"), "version":"1.0.0", "language":"zh-CN", "theme":"light", "fontScale":100, "connected":true,
            "peerLabel":"DESKTOP-STUDIO", "localFingerprint":"AA:BB", "autostart":true, "autostartLastResult":"ok", "autostartLastRun":"2026-10-21 08:30:12",
            "discovery":{"running":true,"issue":"","peersFound":1},
            "discovered":[], "peers":[], "pairing":null, "pairingMessage":"",
            "connection":{"configured":false,"activePeer":"","localEndpoint":"","peerEndpoint":"","autoReceive":true,"valid":true},
            "cross":{"enabled":true,"state":"运行中","placement":"above","cornerGuardEnabled":true,"cornerGuardPixels":198,"requireSecondPush":true,"pauseOnFullscreen":false,"latency":"1.8 ms","admin":false},
            "shared":{"enabled":true,"lastDirection":"","lastFormats":"","lastBytes":"","lastUnits":0},
            "transfer":{"pendingOffer":false,"autoPopup":true,"historyLimit":20,"rowHeight":76,"tasks":[]},
            "recentManifest":null, "lastError":"", "logPath":"clipferry.log"
        })
        .to_string();
        let window = SettingsWindow::create(
            HWND::default(),
            HICON::default(),
            SettingsWindowCommands { dispatch: 1001 },
            &root,
        )
        .unwrap();
        window.update_json(model).unwrap();
        window.show("about").unwrap();
        let mut metrics = serde_json::Value::Null;
        for _ in 0..100 {
            metrics = evaluate_webview(
                &window,
                "[document.querySelectorAll('.nav-button').length,document.body.innerText.includes('版本 1.0.0'),document.querySelector('.about-logo')?.src.startsWith('data:image/png;base64,')]",
            );
            if metrics[0].as_u64() == Some(10) && metrics[1].as_bool() == Some(true) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // Both builds show ten pages: licensing builds swap "Support the author" for the license page.
        assert_eq!(metrics, serde_json::json!([10, true, true]));
        #[cfg(feature = "licensing")]
        {
            assert_license_page(&window);
            window.show("support").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(300));
            let official = evaluate_webview(
                &window,
                "[!!document.querySelector('[data-page=support]'),document.querySelectorAll('.support-qr-preview').length,document.body.innerText.includes('支持作者')]",
            );
            assert_eq!(official, serde_json::json!([false, 0, false]));
        }
        #[cfg(not(feature = "licensing"))]
        {
            window.show("license").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(300));
            let community = evaluate_webview(
                &window,
                "[!!document.querySelector('[data-page=license]'),document.body.innerText.includes('永久授权'),!!document.querySelector('#license-code')]",
            );
            assert_eq!(community, serde_json::json!([false, false, false]));

            window.show("support").unwrap();
            let mut support = serde_json::Value::Null;
            for _ in 0..100 {
                support = evaluate_webview(
                    &window,
                    "[!!document.querySelector('.nav-button.active[data-page=support]'),document.querySelectorAll('.support-qr-preview').length,[...document.querySelectorAll('.support-qr-preview')].every(i=>i.src.startsWith('data:image/jpeg;base64,')),document.body.innerText.includes('感谢你使用 Pierdesk')]",
                );
                if support[1].as_u64() == Some(2) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert_eq!(support, serde_json::json!([true, 2, true, true]));
            let zoomed = evaluate_webview(
                &window,
                "(()=>{document.querySelector('[data-action=support-qr][data-provider=wechat]').click();return 0})()",
            );
            assert_eq!(zoomed, serde_json::json!(0));
            std::thread::sleep(std::time::Duration::from_millis(300));
            let modal = evaluate_webview(
                &window,
                "[document.querySelector('.support-qr-large')?.src.startsWith('data:image/jpeg;base64,'),document.querySelector('.support-qr-modal')?.innerText.includes('感谢支持')]",
            );
            assert_eq!(modal, serde_json::json!([true, true]));
            evaluate_webview(
                &window,
                "(()=>{document.querySelector('.support-qr-modal [data-action=close-modal]').click();return 0})()",
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        window.show("remote").unwrap();
        let mut remote = serde_json::Value::Null;
        for _ in 0..100 {
            remote = evaluate_webview(
                &window,
                "[document.querySelector('.ssh-workspace')!==null,document.querySelector('.ssh-local')!==null,document.querySelectorAll('input[type=password]').length]",
            );
            if remote[0].as_bool() == Some(true) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(remote, serde_json::json!([true, true, 0]));
        window.show("cross").unwrap();
        let mut boundary_editor = serde_json::Value::Null;
        for _ in 0..100 {
            boundary_editor = evaluate_webview(
                &window,
                "[document.querySelector('.boundary-v3-spatial-stage')!==null,document.querySelectorAll('.boundary-v3-step').length,document.body.innerText.includes('空间布局')]",
            );
            if boundary_editor[0].as_bool() == Some(true) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(boundary_editor, serde_json::json!([true, 3, true]));
        assert_boundary_save_receipt_unlocks_editor(&window);
        assert_boundary_revision_normalization(&window);
        let graph = evaluate_webview(
            &window,
            "document.querySelector('[data-v3-view=\"boundary\"]')?.click();[document.querySelector('.boundary-v3-edit-screen .boundary-v3-graph-stage.embedded')!==null,document.body.innerText.includes('连接关系')]",
        );
        assert_eq!(graph, serde_json::json!([true, true]));
        assert_boundary_graph_persists_across_webview_recreation(&window, &root);
        window.show("general").unwrap();
        let mut font_scale = serde_json::Value::Null;
        for _ in 0..100 {
            font_scale = evaluate_webview(
                &window,
                "[document.querySelector('#font-scale')?.min,document.querySelector('#font-scale')?.max]",
            );
            if font_scale[0].as_str() == Some("50") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(font_scale, serde_json::json!(["50", "125"]));
        assert_english_dynamic_text_is_localized(&window);
        assert!(window.state.webview.borrow().is_some());
        window.state.hide(window.handle);
        assert!(window.state.webview.borrow().is_none());
        window.show("status").unwrap();
        assert!(window.state.webview.borrow().is_some());
        drop(window);
        assert!(root.starts_with(std::env::temp_dir()));
        let _ = std::fs::remove_dir_all(root);
    }
}
