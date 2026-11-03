//! Global double-right-Alt pointer locator and its click-through animation window.
use std::io;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, mpsc};
use std::thread::{self, JoinHandle};

use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreatePen, CreateSolidBrush, DeleteObject, Ellipse, EndPaint, FillRect, HGDIOBJ,
    HOLLOW_BRUSH, InvalidateRect, PAINTSTRUCT, PS_SOLID, SelectObject,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    VK_CONTROL, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_RCONTROL, VK_RMENU,
    VK_RSHIFT, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos,
    GetMessageW, HC_ACTION, HHOOK, HWND_TOPMOST, KBDLLHOOKSTRUCT, KillTimer, LWA_COLORKEY, MSG,
    PostThreadMessageW, RegisterClassW, SW_HIDE, SWP_NOACTIVATE, SWP_SHOWWINDOW,
    SetLayeredWindowAttributes, SetTimer, SetWindowPos, SetWindowsHookExW, TranslateMessage,
    UnhookWindowsHookEx, UnregisterClassW, WH_KEYBOARD_LL, WM_APP, WM_ERASEBKGND, WM_KEYDOWN,
    WM_KEYUP, WM_PAINT, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER, WNDCLASSW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows_core::{PCWSTR, w};

const WINDOW_CLASS: PCWSTR = w!("PierdeskCursorLocatorWindow");
const WINDOW_TITLE: PCWSTR = w!("Pierdesk Pointer Locator");
const LOCATE_MESSAGE: u32 = WM_APP + 0x246;
const TIMER_ID: usize = 0x4346_4C4F;
const TIMER_INTERVAL_MS: u32 = 16;
const ANIMATION_DURATION_MS: u64 = 680;
const DOUBLE_TAP_WINDOW_MS: u64 = 500;
const WINDOW_EDGE: i32 = 240;

static LOCATOR_THREAD_ID: AtomicU32 = AtomicU32::new(0);
static DOUBLE_TAP: Mutex<DoubleRightAlt> = Mutex::new(DoubleRightAlt::new());
static ANIMATION: Mutex<Animation> = Mutex::new(Animation::new());

#[derive(Clone, Copy)]
struct DoubleRightAlt {
    pressed: bool,
    last_release_ms: Option<u64>,
    ignore_release: bool,
}

impl DoubleRightAlt {
    const fn new() -> Self {
        Self {
            pressed: false,
            last_release_ms: None,
            ignore_release: false,
        }
    }

    fn input(&mut self, virtual_key: u32, message: u32, now_ms: u64) -> bool {
        let down = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
        let up = matches!(message, WM_KEYUP | WM_SYSKEYUP);
        if virtual_key != u32::from(VK_RMENU.0) {
            if down && !is_modifier_key(virtual_key) {
                self.last_release_ms = None;
                self.ignore_release = false;
            }
            return false;
        }
        if down {
            if self.pressed {
                return false;
            }
            self.pressed = true;
            let triggered = self
                .last_release_ms
                .is_some_and(|last| now_ms.saturating_sub(last) <= DOUBLE_TAP_WINDOW_MS);
            if triggered {
                self.last_release_ms = None;
                self.ignore_release = true;
            }
            return triggered;
        }
        if up {
            self.pressed = false;
            if self.ignore_release {
                self.ignore_release = false;
            } else {
                self.last_release_ms = Some(now_ms);
            }
        }
        false
    }

    fn reset(&mut self) {
        *self = Self::new();
    }
}

fn is_modifier_key(virtual_key: u32) -> bool {
    [
        VK_CONTROL,
        VK_LCONTROL,
        VK_RCONTROL,
        VK_SHIFT,
        VK_LSHIFT,
        VK_RSHIFT,
        VK_MENU,
        VK_LMENU,
        VK_LWIN,
        VK_RWIN,
    ]
    .into_iter()
    .any(|key| virtual_key == u32::from(key.0))
}

#[derive(Clone, Copy)]
struct Animation {
    active: bool,
    started_ms: u64,
}

impl Animation {
    const fn new() -> Self {
        Self {
            active: false,
            started_ms: 0,
        }
    }
}

pub(crate) struct CursorLocatorRuntime {
    thread_id: u32,
    worker: Option<JoinHandle<()>>,
}

impl CursorLocatorRuntime {
    pub(crate) fn start() -> io::Result<Self> {
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("pierdesk-cursor-locator".to_owned())
            .spawn(move || locator_thread(&ready_tx))?;
        match ready_rx.recv() {
            Ok(Ok(thread_id)) => Ok(Self {
                thread_id,
                worker: Some(worker),
            }),
            Ok(Err(error)) => {
                let _ = worker.join();
                Err(error)
            }
            Err(error) => {
                let _ = worker.join();
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    format!("cursor locator did not initialize: {error}"),
                ))
            }
        }
    }

    pub(crate) fn stop(&mut self) {
        if self.thread_id != 0 {
            // SAFETY: the locator thread creates a message queue before reporting readiness.
            let _ = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
            self.thread_id = 0;
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for CursorLocatorRuntime {
    fn drop(&mut self) {
        self.stop();
    }
}

struct LocatorResources {
    window: HWND,
    instance: HINSTANCE,
    hook: HHOOK,
    background: windows::Win32::Graphics::Gdi::HBRUSH,
}

impl Drop for LocatorResources {
    fn drop(&mut self) {
        // SAFETY: every handle is owned by this locator thread and released exactly once.
        let _ = unsafe { UnhookWindowsHookEx(self.hook) };
        let _ = unsafe { KillTimer(Some(self.window), TIMER_ID) };
        let _ = unsafe { DestroyWindow(self.window) };
        let _ = unsafe { UnregisterClassW(WINDOW_CLASS, Some(self.instance)) };
        let _ = unsafe { DeleteObject(HGDIOBJ(self.background.0)) };
    }
}

fn locator_thread(ready: &mpsc::SyncSender<io::Result<u32>>) {
    let resources = match create_resources() {
        Ok(resources) => resources,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    // SAFETY: this thread now owns a window and therefore has an initialized message queue.
    let thread_id = unsafe { GetCurrentThreadId() };
    LOCATOR_THREAD_ID.store(thread_id, Ordering::Release);
    DOUBLE_TAP
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .reset();
    if ready.send(Ok(thread_id)).is_err() {
        LOCATOR_THREAD_ID.store(0, Ordering::Release);
        return;
    }
    loop {
        let mut message = MSG::default();
        // SAFETY: message is writable and this thread owns the queue.
        let result = unsafe { GetMessageW(&raw mut message, None, 0, 0) }.0;
        if result <= 0 {
            break;
        }
        if message.message == LOCATE_MESSAGE {
            let _ = show_locator(resources.window);
            continue;
        }
        // SAFETY: GetMessageW initialized the message.
        unsafe {
            let _ = TranslateMessage(&raw const message);
            DispatchMessageW(&raw const message);
        }
    }
    LOCATOR_THREAD_ID.store(0, Ordering::Release);
    ANIMATION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .active = false;
}

fn create_resources() -> io::Result<LocatorResources> {
    // SAFETY: the current executable module remains loaded for the process lifetime.
    let module = unsafe { GetModuleHandleW(None) }.map_err(io::Error::other)?;
    let instance = HINSTANCE(module.0);
    // SAFETY: CreateSolidBrush returns an owned GDI brush used for this class only.
    let background = unsafe { CreateSolidBrush(COLORREF(0)) };
    if background.0.is_null() {
        return Err(io::Error::last_os_error());
    }
    let class = WNDCLASSW {
        lpfnWndProc: Some(locator_window_procedure),
        hInstance: instance,
        hbrBackground: background,
        lpszClassName: WINDOW_CLASS,
        ..WNDCLASSW::default()
    };
    // SAFETY: the class descriptor and static strings remain valid.
    if unsafe { RegisterClassW(&raw const class) } == 0 {
        let _ = unsafe { DeleteObject(HGDIOBJ(background.0)) };
        return Err(io::Error::last_os_error());
    }
    // SAFETY: creates one hidden, non-activating, click-through top-level overlay.
    let window = match unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_TRANSPARENT,
            WINDOW_CLASS,
            WINDOW_TITLE,
            WS_POPUP,
            0,
            0,
            WINDOW_EDGE,
            WINDOW_EDGE,
            None,
            None,
            Some(instance),
            None,
        )
    } {
        Ok(window) => window,
        Err(error) => {
            let _ = unsafe { UnregisterClassW(WINDOW_CLASS, Some(instance)) };
            let _ = unsafe { DeleteObject(HGDIOBJ(background.0)) };
            return Err(io::Error::other(error));
        }
    };
    if let Err(error) =
        unsafe { SetLayeredWindowAttributes(window, COLORREF(0), 255, LWA_COLORKEY) }
    {
        let _ = unsafe { DestroyWindow(window) };
        let _ = unsafe { UnregisterClassW(WINDOW_CLASS, Some(instance)) };
        let _ = unsafe { DeleteObject(HGDIOBJ(background.0)) };
        return Err(io::Error::other(error));
    }
    // SAFETY: the callback has the system ABI and remains loaded for the process lifetime.
    let hook = match unsafe {
        SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(locator_keyboard_hook),
            Some(instance),
            0,
        )
    } {
        Ok(hook) => hook,
        Err(error) => {
            let _ = unsafe { DestroyWindow(window) };
            let _ = unsafe { UnregisterClassW(WINDOW_CLASS, Some(instance)) };
            let _ = unsafe { DeleteObject(HGDIOBJ(background.0)) };
            return Err(io::Error::other(error));
        }
    };
    Ok(LocatorResources {
        window,
        instance,
        hook,
        background,
    })
}

fn show_locator(window: HWND) -> io::Result<()> {
    let mut cursor = POINT::default();
    // SAFETY: cursor points to writable storage.
    unsafe { GetCursorPos(&raw mut cursor) }.map_err(io::Error::other)?;
    let now = unsafe { GetTickCount64() };
    *ANIMATION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Animation {
        active: true,
        started_ms: now,
    };
    // SAFETY: the locator owns the live top-level window and never activates it.
    unsafe {
        SetWindowPos(
            window,
            Some(HWND_TOPMOST),
            cursor.x - WINDOW_EDGE / 2,
            cursor.y - WINDOW_EDGE / 2,
            WINDOW_EDGE,
            WINDOW_EDGE,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        )
    }
    .map_err(io::Error::other)?;
    if unsafe { SetTimer(Some(window), TIMER_ID, TIMER_INTERVAL_MS, None) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let _ = unsafe { InvalidateRect(Some(window), None, true) };
    Ok(())
}

unsafe extern "system" fn locator_keyboard_hook(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code == i32::try_from(HC_ACTION).unwrap_or_default() && lparam.0 != 0 {
        // SAFETY: HC_ACTION provides a valid KBDLLHOOKSTRUCT for this callback invocation.
        let data = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let message = u32::try_from(wparam.0).unwrap_or_default();
        let now = unsafe { GetTickCount64() };
        let triggered = DOUBLE_TAP
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .input(data.vkCode, message, now);
        if triggered {
            let thread_id = LOCATOR_THREAD_ID.load(Ordering::Acquire);
            if thread_id != 0 {
                // SAFETY: the target is the locator thread's pointer-free message queue.
                let _ =
                    unsafe { PostThreadMessageW(thread_id, LOCATE_MESSAGE, WPARAM(0), LPARAM(0)) };
            }
        }
    }
    // SAFETY: the shortcut observes input and always preserves the existing hook chain.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

unsafe extern "system" fn locator_window_procedure(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_ERASEBKGND => return LRESULT(1),
        WM_TIMER if wparam.0 == TIMER_ID => {
            let now = unsafe { GetTickCount64() };
            let mut animation = ANIMATION
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if animation.active && now.saturating_sub(animation.started_ms) >= ANIMATION_DURATION_MS
            {
                animation.active = false;
                let _ = unsafe { KillTimer(Some(window), TIMER_ID) };
                let _ =
                    unsafe { windows::Win32::UI::WindowsAndMessaging::ShowWindow(window, SW_HIDE) };
            } else if animation.active {
                let _ = unsafe { InvalidateRect(Some(window), None, true) };
            }
            return LRESULT(0);
        }
        WM_PAINT => {
            paint_locator(window);
            return LRESULT(0);
        }
        _ => {}
    }
    // SAFETY: all unhandled messages retain default top-level-window behavior.
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

fn paint_locator(window: HWND) {
    let animation = *ANIMATION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = unsafe { GetTickCount64() };
    let elapsed = now
        .saturating_sub(animation.started_ms)
        .min(ANIMATION_DURATION_MS);
    let mut paint = PAINTSTRUCT::default();
    // SAFETY: this is called only while handling WM_PAINT for the live locator window.
    let dc = unsafe { BeginPaint(window, &raw mut paint) };
    let background = unsafe { CreateSolidBrush(COLORREF(0)) };
    let rect = RECT {
        left: 0,
        top: 0,
        right: WINDOW_EDGE,
        bottom: WINDOW_EDGE,
    };
    let _ = unsafe { FillRect(dc, &raw const rect, background) };
    let old_brush = unsafe {
        SelectObject(
            dc,
            windows::Win32::Graphics::Gdi::GetStockObject(HOLLOW_BRUSH),
        )
    };
    let center = WINDOW_EDGE / 2;
    for (index, initial_radius) in [112_i32, 94, 76, 58, 40].into_iter().enumerate() {
        let remaining = ANIMATION_DURATION_MS.saturating_sub(elapsed);
        let radius = 16
            + i32::try_from(
                u64::try_from(initial_radius - 16).unwrap_or_default() * remaining
                    / ANIMATION_DURATION_MS,
            )
            .unwrap_or_default();
        let colors = [
            COLORREF(0x00FF_AA48),
            COLORREF(0x00FF_BB55),
            COLORREF(0x00FF_CC66),
            COLORREF(0x00EE_DD77),
            COLORREF(0x00DD_EE88),
        ];
        let pen = unsafe { CreatePen(PS_SOLID, 3, colors[index]) };
        let old_pen = unsafe { SelectObject(dc, HGDIOBJ(pen.0)) };
        let _ = unsafe {
            Ellipse(
                dc,
                center - radius,
                center - radius,
                center + radius,
                center + radius,
            )
        };
        let _ = unsafe { SelectObject(dc, old_pen) };
        let _ = unsafe { DeleteObject(HGDIOBJ(pen.0)) };
    }
    let _ = unsafe { SelectObject(dc, old_brush) };
    let _ = unsafe { DeleteObject(HGDIOBJ(background.0)) };
    // SAFETY: balances BeginPaint for this WM_PAINT call.
    let _ = unsafe { EndPaint(window, &raw const paint) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn right_alt_double_tap_triggers_once_and_ignores_repeat() {
        let mut detector = DoubleRightAlt::new();
        assert!(!detector.input(u32::from(VK_RMENU.0), WM_SYSKEYDOWN, 100));
        assert!(!detector.input(u32::from(VK_RMENU.0), WM_SYSKEYDOWN, 110));
        assert!(!detector.input(u32::from(VK_RMENU.0), WM_SYSKEYUP, 140));
        assert!(detector.input(u32::from(VK_RMENU.0), WM_SYSKEYDOWN, 420));
        assert!(!detector.input(u32::from(VK_RMENU.0), WM_SYSKEYUP, 450));
        assert!(!detector.input(u32::from(VK_RMENU.0), WM_SYSKEYDOWN, 490));
    }

    #[test]
    fn slow_or_interrupted_right_alt_taps_do_not_trigger() {
        let mut detector = DoubleRightAlt::new();
        assert!(!detector.input(u32::from(VK_RMENU.0), WM_KEYDOWN, 100));
        assert!(!detector.input(u32::from(VK_RMENU.0), WM_KEYUP, 120));
        assert!(!detector.input(u32::from(VK_RMENU.0), WM_KEYDOWN, 700));
        assert!(!detector.input(u32::from(VK_RMENU.0), WM_KEYUP, 720));

        let mut interrupted = DoubleRightAlt::new();
        assert!(!interrupted.input(u32::from(VK_RMENU.0), WM_KEYDOWN, 800));
        assert!(!interrupted.input(u32::from(VK_RMENU.0), WM_KEYUP, 820));
        assert!(!interrupted.input(0x41, WM_KEYDOWN, 850));
        assert!(!interrupted.input(u32::from(VK_RMENU.0), WM_KEYDOWN, 900));
    }

    #[test]
    fn altgr_control_events_do_not_break_right_alt_double_tap() {
        let mut detector = DoubleRightAlt::new();
        assert!(!detector.input(u32::from(VK_LCONTROL.0), WM_KEYDOWN, 90));
        assert!(!detector.input(u32::from(VK_RMENU.0), WM_SYSKEYDOWN, 100));
        assert!(!detector.input(u32::from(VK_RMENU.0), WM_SYSKEYUP, 120));
        assert!(!detector.input(u32::from(VK_LCONTROL.0), WM_KEYUP, 125));
        assert!(!detector.input(u32::from(VK_LCONTROL.0), WM_KEYDOWN, 280));
        assert!(detector.input(u32::from(VK_RMENU.0), WM_SYSKEYDOWN, 290));
    }

    #[test]
    fn locator_runtime_installs_and_stops_its_native_hook() {
        let mut runtime = CursorLocatorRuntime::start().unwrap();
        assert_ne!(runtime.thread_id, 0);
        assert_eq!(LOCATOR_THREAD_ID.load(Ordering::Acquire), runtime.thread_id);
        runtime.stop();
        assert_eq!(LOCATOR_THREAD_ID.load(Ordering::Acquire), 0);
    }
}
