use std::ffi::{OsStr, c_void};
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt as _;

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    ICC_BAR_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx, TBM_SETPAGESIZE, TBM_SETPOS,
    TBM_SETRANGE, TBM_SETTICFREQ, TRACKBAR_CLASSW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    BS_DEFPUSHBUTTON, BS_PUSHBUTTON, CREATESTRUCTW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW,
    DestroyWindow, DispatchMessageW, GWLP_USERDATA, GetMessageW, GetWindowLongPtrW, HMENU,
    IDC_ARROW, IsDialogMessageW, LoadCursorW, MSG, PostQuitMessage, RegisterClassW, SW_SHOW,
    SendMessageW, SetForegroundWindow, SetWindowLongPtrW, SetWindowTextW, ShowWindow,
    TranslateMessage, UnregisterClassW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_COMMAND,
    WM_HSCROLL, WM_NCCREATE, WM_NCDESTROY, WM_USER, WNDCLASSW, WS_CAPTION, WS_CHILD,
    WS_CLIPCHILDREN, WS_EX_DLGMODALFRAME, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};
use windows_core::{PCWSTR, w};

const WINDOW_CLASS: PCWSTR = w!("PierdeskCornerGuardWindow");
const CONTROL_OK: usize = 1;
const CONTROL_CANCEL: usize = 2;
const MIN_PIXELS: u32 = 0;
const MAX_PIXELS: u32 = 512;
// TBM_GETPOS is numerically equal to WM_USER, but windows-rs does not expose it in
// every generated feature combination.
const TBM_GETPOS_MESSAGE: u32 = WM_USER;

struct ProcedureState {
    trackbar: HWND,
    value_label: HWND,
    result: Option<u32>,
    done: bool,
}

/// Shows a native modal trackbar and returns the confirmed local corner-guard size.
pub(crate) fn show(owner: HWND, initial: u32) -> io::Result<Option<u32>> {
    let controls = INITCOMMONCONTROLSEX {
        dwSize: u32::try_from(size_of::<INITCOMMONCONTROLSEX>()).unwrap_or(u32::MAX),
        dwICC: ICC_BAR_CLASSES,
    };
    if !unsafe { InitCommonControlsEx(&raw const controls) }.as_bool() {
        return Err(io::Error::last_os_error());
    }
    let module = unsafe { GetModuleHandleW(None) }.map_err(windows_error)?;
    let instance = HINSTANCE(module.0);
    let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }.map_err(windows_error)?;
    let class = WNDCLASSW {
        lpfnWndProc: Some(window_procedure),
        hInstance: instance,
        hCursor: cursor,
        lpszClassName: WINDOW_CLASS,
        ..Default::default()
    };
    if unsafe { RegisterClassW(&raw const class) } == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut state = Box::new(ProcedureState {
        trackbar: HWND::default(),
        value_label: HWND::default(),
        result: None,
        done: false,
    });
    let state_pointer = (&raw mut *state).cast::<c_void>();
    let window = match unsafe {
        CreateWindowExW(
            WS_EX_DLGMODALFRAME,
            WINDOW_CLASS,
            w!("Pierdesk 四角保护范围"),
            WS_CAPTION | WS_SYSMENU | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            500,
            220,
            None,
            None,
            Some(instance),
            Some(state_pointer.cast_const()),
        )
    } {
        Ok(window) => window,
        Err(error) => {
            let _ = unsafe { UnregisterClassW(WINDOW_CLASS, Some(instance)) };
            return Err(windows_error(error));
        }
    };

    let setup = create_controls(window, instance, initial.min(MAX_PIXELS), &mut state);
    if let Err(error) = setup {
        let _ = unsafe { DestroyWindow(window) };
        let _ = unsafe { UnregisterClassW(WINDOW_CLASS, Some(instance)) };
        return Err(error);
    }
    let _ = unsafe { EnableWindow(owner, false) };
    let _ = unsafe { ShowWindow(window, SW_SHOW) };
    let _ = unsafe { SetForegroundWindow(window) };

    let mut repost_quit = None;
    loop {
        if state.done {
            break;
        }
        let mut message = MSG::default();
        let result = unsafe { GetMessageW(&raw mut message, None, 0, 0) }.0;
        if result == -1 {
            let _ = unsafe { EnableWindow(owner, true) };
            let _ = unsafe { DestroyWindow(window) };
            let _ = unsafe { UnregisterClassW(WINDOW_CLASS, Some(instance)) };
            return Err(io::Error::last_os_error());
        }
        if result == 0 {
            repost_quit = Some(i32::try_from(message.wParam.0).unwrap_or_default());
            break;
        }
        if !unsafe { IsDialogMessageW(window, &raw const message) }.as_bool() {
            unsafe {
                let _ = TranslateMessage(&raw const message);
                DispatchMessageW(&raw const message);
            }
        }
    }
    let result = state.result;
    let _ = unsafe { EnableWindow(owner, true) };
    let _ = unsafe { SetForegroundWindow(owner) };
    let _ = unsafe { DestroyWindow(window) };
    let _ = unsafe { UnregisterClassW(WINDOW_CLASS, Some(instance)) };
    if let Some(exit_code) = repost_quit {
        unsafe { PostQuitMessage(exit_code) };
    }
    Ok(result)
}

fn create_controls(
    parent: HWND,
    instance: HINSTANCE,
    initial: u32,
    state: &mut ProcedureState,
) -> io::Result<()> {
    let _ = create_child(
        parent,
        instance,
        w!("STATIC"),
        "拖动滑块设置本机鼠标跨出屏幕时，两端四角各自保留的保护范围。",
        24,
        18,
        440,
        22,
        0,
        WINDOW_STYLE::default(),
    )?;
    state.value_label = create_child(
        parent,
        instance,
        w!("STATIC"),
        &value_text(initial),
        24,
        48,
        440,
        22,
        0,
        WINDOW_STYLE::default(),
    )?;
    state.trackbar = create_child(
        parent,
        instance,
        TRACKBAR_CLASSW,
        "",
        24,
        76,
        440,
        46,
        0,
        WS_TABSTOP,
    )?;
    unsafe {
        SendMessageW(
            state.trackbar,
            TBM_SETRANGE,
            Some(WPARAM(1)),
            Some(LPARAM(
                isize::try_from(MAX_PIXELS << 16).unwrap_or(isize::MAX),
            )),
        );
        SendMessageW(
            state.trackbar,
            TBM_SETPAGESIZE,
            Some(WPARAM(0)),
            Some(LPARAM(16)),
        );
        SendMessageW(
            state.trackbar,
            TBM_SETTICFREQ,
            Some(WPARAM(32)),
            Some(LPARAM(0)),
        );
        SendMessageW(
            state.trackbar,
            TBM_SETPOS,
            Some(WPARAM(1)),
            Some(LPARAM(isize::try_from(initial).unwrap_or_default())),
        );
    }
    let _ = create_child(
        parent,
        instance,
        w!("BUTTON"),
        "确定",
        278,
        132,
        88,
        30,
        CONTROL_OK,
        WS_TABSTOP | WINDOW_STYLE(BS_DEFPUSHBUTTON as u32),
    )?;
    let _ = create_child(
        parent,
        instance,
        w!("BUTTON"),
        "取消",
        376,
        132,
        88,
        30,
        CONTROL_CANCEL,
        WS_TABSTOP | WINDOW_STYLE(BS_PUSHBUTTON as u32),
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn create_child(
    parent: HWND,
    instance: HINSTANCE,
    class_name: PCWSTR,
    text: &str,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    identifier: usize,
    extra_style: WINDOW_STYLE,
) -> io::Result<HWND> {
    let text = wide_null(text.as_ref());
    let menu = (identifier != 0).then_some(HMENU(identifier as *mut c_void));
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            PCWSTR(text.as_ptr()),
            WS_CHILD | WS_VISIBLE | extra_style,
            x,
            y,
            width,
            height,
            Some(parent),
            menu,
            Some(instance),
            None,
        )
    }
    .map_err(windows_error)
}

unsafe extern "system" fn window_procedure(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = lparam.0 as *const CREATESTRUCTW;
        if !create.is_null() {
            let state = unsafe { (*create).lpCreateParams } as isize;
            unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, state) };
        }
    }
    let state = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *mut ProcedureState;
    if !state.is_null() {
        let state = unsafe { &mut *state };
        match message {
            WM_HSCROLL => {
                update_value_label(state);
                return LRESULT(0);
            }
            WM_COMMAND => {
                match wparam.0 & 0xffff {
                    CONTROL_OK => {
                        state.result = Some(trackbar_position(state.trackbar));
                        state.done = true;
                        let _ = unsafe { DestroyWindow(window) };
                    }
                    CONTROL_CANCEL => {
                        state.done = true;
                        let _ = unsafe { DestroyWindow(window) };
                    }
                    _ => {}
                }
                return LRESULT(0);
            }
            WM_CLOSE => {
                state.done = true;
                let _ = unsafe { DestroyWindow(window) };
                return LRESULT(0);
            }
            WM_NCDESTROY => {
                unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, 0) };
            }
            _ => {}
        }
    }
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

fn update_value_label(state: &ProcedureState) {
    let text = wide_null(value_text(trackbar_position(state.trackbar)).as_ref());
    let _ = unsafe { SetWindowTextW(state.value_label, PCWSTR(text.as_ptr())) };
}

fn trackbar_position(trackbar: HWND) -> u32 {
    let position = unsafe { SendMessageW(trackbar, TBM_GETPOS_MESSAGE, None, None) }.0;
    u32::try_from(position).unwrap_or_default().min(MAX_PIXELS)
}

fn value_text(value: u32) -> String {
    if value == MIN_PIXELS {
        "当前：0 像素（不保留四角保护区）".to_owned()
    } else {
        format!("当前：{value} 像素（可用范围 0–{MAX_PIXELS}）")
    }
}

fn wide_null(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

fn windows_error(error: windows_core::Error) -> io::Error {
    io::Error::other(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_cover_disabled_and_maximum_values() {
        assert!(value_text(0).contains("不保留"));
        assert!(value_text(512).contains("512"));
    }
}
