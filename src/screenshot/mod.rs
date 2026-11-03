//! On-demand local capture. The normal forwarded keyboard path triggers the
//! target's registered hotkey; screenshot pixels use the existing clipboard path.
use std::io;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;

use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HWND, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, RegisterHotKey,
    UnregisterHotKey,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, GetWindowThreadProcessId, PostMessageW, WM_CLOSE,
};
use windows_core::w;

mod candidates;
mod visual;
mod window;

const EDIT_MUTEX: windows_core::PCWSTR = w!("Local\\ClipFerry.Screenshot.Selection.v1");
const DEFAULT_SHORTCUT: &str = "Alt+X";
const SETTINGS_FILE: &str = "screenshot-shortcut.v1";

// A kernel mutex handle can be queried from any thread. Ownership is only
// acquired/released on the querying thread, never transferred between threads.
struct Gate(HANDLE);
unsafe impl Send for Gate {}
unsafe impl Sync for Gate {}
impl Drop for Gate {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}
static GATE: OnceLock<Option<Gate>> = OnceLock::new();

fn gate() -> Option<&'static Gate> {
    GATE.get_or_init(|| {
        unsafe { CreateMutexW(None, false, EDIT_MUTEX) }
            .ok()
            .map(Gate)
    })
    .as_ref()
}

/// A kernel-owned lock disappears on helper crash, so a stale screenshot can
/// never permanently disable boundary switching in the tray process.
pub(crate) fn selection_active() -> bool {
    gate().is_some_and(|gate| match unsafe { WaitForSingleObject(gate.0, 0) } {
        WAIT_TIMEOUT => true,
        WAIT_OBJECT_0 | WAIT_ABANDONED => {
            let _ = unsafe { ReleaseMutex(gate.0) };
            false
        }
        _ => false,
    })
}

struct SelectionGuard;
impl SelectionGuard {
    fn acquire() -> io::Result<Option<Self>> {
        let gate = gate().ok_or_else(io::Error::last_os_error)?;
        match unsafe { WaitForSingleObject(gate.0, 0) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Some(Self)),
            WAIT_TIMEOUT => Ok(None),
            _ => Err(io::Error::last_os_error()),
        }
    }
}
impl Drop for SelectionGuard {
    fn drop(&mut self) {
        if let Some(gate) = gate() {
            let _ = unsafe { ReleaseMutex(gate.0) };
        }
    }
}

pub(crate) struct Manager {
    pub shortcut: String,
    pub error: String,
    hotkey_id: i32,
    children: Vec<Child>,
    warm: Option<Child>,
    warm_retry: std::time::Instant,
    stopping: bool,
}
impl Manager {
    pub fn new(root: &Path) -> Self {
        let shortcut = std::fs::read_to_string(root.join(SETTINGS_FILE))
            .ok()
            .filter(|s| parse_shortcut(s.trim()).is_ok())
            .unwrap_or_else(|| DEFAULT_SHORTCUT.into());
        Self {
            shortcut: shortcut.trim().into(),
            error: String::new(),
            hotkey_id: 0x4340,
            children: Vec::new(),
            warm: None,
            warm_retry: std::time::Instant::now(),
            stopping: false,
        }
    }
    pub fn bind(&mut self, window: HWND) {
        self.error = Self::register(window, self.hotkey_id, &self.shortcut)
            .err()
            .map_or_else(String::new, |e| e.to_string());
        self.poll();
    }
    fn register(window: HWND, id: i32, value: &str) -> io::Result<()> {
        let (modifiers, key) = parse_shortcut(value)?;
        unsafe { RegisterHotKey(Some(window), id, modifiers | MOD_NOREPEAT, key) }.map_err(|_| {
            io::Error::other(
                "截图快捷键已被占用，请在常规设置中换一个组合 / Screenshot shortcut is in use",
            )
        })
    }
    pub fn change(&mut self, window: HWND, root: &Path, value: &str) -> io::Result<()> {
        let value = value.trim();
        if value == self.shortcut && self.error.is_empty() {
            return Ok(());
        }
        let next_id = if self.hotkey_id == 0x4340 {
            0x4341
        } else {
            0x4340
        };
        Self::register(window, next_id, value)?;
        if let Err(error) = std::fs::write(root.join(SETTINGS_FILE), value) {
            let _ = unsafe { UnregisterHotKey(Some(window), next_id) };
            return Err(error);
        }
        let _ = unsafe { UnregisterHotKey(Some(window), self.hotkey_id) };
        self.hotkey_id = next_id;
        self.shortcut = value.into();
        self.error.clear();
        Ok(())
    }
    pub fn matches(&self, id: usize) -> bool {
        usize::try_from(self.hotkey_id) == Ok(id)
    }
    pub fn launch(&mut self, language: &str) -> io::Result<()> {
        self.poll();
        if selection_active() {
            return Ok(());
        }
        if self.children.len() >= 12 {
            return Err(io::Error::other(
                "请先关闭部分贴图 / Close some pinned screenshots first",
            ));
        }
        if let Some(handle) = self
            .warm
            .as_ref()
            .and_then(|child| child_window(child.id()))
            && unsafe {
                PostMessageW(
                    Some(handle),
                    window::ACTIVATE,
                    WPARAM(usize::from(language == "en")),
                    LPARAM(0),
                )
            }
            .is_ok()
        {
            if let Some(child) = self.warm.take() {
                self.children.push(child);
            }
            self.poll();
            return Ok(());
        }
        let child = Command::new(std::env::current_exe()?)
            .args(["screenshot-helper", language])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        self.children.push(child);
        Ok(())
    }
    pub fn poll(&mut self) {
        self.children
            .retain_mut(|child| !matches!(child.try_wait(), Ok(Some(_))));
        if self
            .warm
            .as_mut()
            .is_some_and(|child| matches!(child.try_wait(), Ok(Some(_))))
        {
            self.warm = None;
            self.warm_retry = std::time::Instant::now() + std::time::Duration::from_secs(30);
        }
        if !self.stopping
            && self.warm.is_none()
            && self.children.len() < 12
            && std::time::Instant::now() >= self.warm_retry
        {
            match Command::new(std::env::current_exe().unwrap_or_default())
                .args(["screenshot-warm-helper"])
                .arg(std::process::id().to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => self.warm = Some(child),
                Err(_) => {
                    self.warm_retry =
                        std::time::Instant::now() + std::time::Duration::from_secs(30);
                }
            }
        }
    }
    pub fn unbind(&self, window: HWND) {
        let _ = unsafe { UnregisterHotKey(Some(window), self.hotkey_id) };
    }
    pub fn shutdown(&mut self) {
        self.stopping = true;
        let ids: Vec<_> = self
            .children
            .iter()
            .chain(self.warm.iter())
            .map(Child::id)
            .collect();
        let mut previous = None;
        for _ in 0..64 {
            let Ok(window) =
                (unsafe { FindWindowExW(None, previous, w!("PierdeskScreenshotWindow"), None) })
            else {
                break;
            };
            let mut pid = 0;
            unsafe {
                GetWindowThreadProcessId(window, Some(&raw mut pid));
            }
            if ids.contains(&pid) {
                let _ = unsafe { PostMessageW(Some(window), WM_CLOSE, WPARAM(0), LPARAM(0)) };
            }
            previous = Some(window);
        }
        self.poll();
        if let Some(mut child) = self.warm.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn child_window(process_id: u32) -> Option<HWND> {
    let mut previous = None;
    for _ in 0..64 {
        let handle =
            unsafe { FindWindowExW(None, previous, w!("PierdeskScreenshotWindow"), None) }.ok()?;
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(handle, Some(&raw mut pid)) };
        if pid == process_id {
            return Some(handle);
        }
        previous = Some(handle);
    }
    None
}

fn parse_shortcut(value: &str) -> io::Result<(HOT_KEY_MODIFIERS, u32)> {
    let invalid = || {
        io::Error::other(
            "快捷键需为 Ctrl/Alt/Shift + 字母或 F1–F11 / Use Ctrl/Alt/Shift + a letter or F1–F11",
        )
    };
    let parts: Vec<_> = value.split('+').collect();
    if !(2..=4).contains(&parts.len()) {
        return Err(invalid());
    }
    let mut modifiers = HOT_KEY_MODIFIERS(0);
    for part in &parts[..parts.len() - 1] {
        let flag = match *part {
            "Ctrl" => MOD_CONTROL,
            "Alt" => MOD_ALT,
            "Shift" => MOD_SHIFT,
            _ => return Err(invalid()),
        };
        if modifiers.0 & flag.0 != 0 {
            return Err(invalid());
        }
        modifiers |= flag;
    }
    if modifiers.0 & (MOD_CONTROL | MOD_ALT).0 == 0 {
        return Err(invalid());
    }
    let key = parts[parts.len() - 1];
    let key = if key.len() == 1 && key.as_bytes()[0].is_ascii_uppercase() {
        u32::from(key.as_bytes()[0])
    } else if let Some(number) = key
        .strip_prefix('F')
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|n| (1..=11).contains(n))
    {
        0x6f + number
    } else {
        return Err(invalid());
    };
    Ok((modifiers, key))
}

/// Opens a screenshot editor in this process. Called only by the GUI helper.
/// # Errors
/// Returns capture, window, or `WebView` initialization failures.
pub fn run(language: &str) -> io::Result<()> {
    let Some(selection) = SelectionGuard::acquire()? else {
        return Ok(());
    };
    window::run(language, selection)
}

/// Prepare one hidden editor without capturing pixels or acquiring the selection lock.
/// # Errors
/// Returns window or `WebView` initialization failures.
pub fn warm(parent_id: u32) -> io::Result<()> {
    window::run_warm(parent_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shortcuts_require_modifiers_and_reject_reserved_or_ambiguous_keys() {
        assert_eq!(parse_shortcut(DEFAULT_SHORTCUT).unwrap(), (MOD_ALT, 0x58));
        assert_eq!(parse_shortcut("Ctrl+Alt+A").unwrap().1, 65);
        for bad in [
            "A",
            "Shift+A",
            "Ctrl+F12",
            "Ctrl+Ctrl+A",
            "Win+S",
            "Ctrl+é",
            "Ctrl+Alt",
            "Ctrl+F0",
        ] {
            assert!(parse_shortcut(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn new_default_preserves_a_saved_custom_shortcut() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(Manager::new(root.path()).shortcut, "Alt+X");
        std::fs::write(root.path().join(SETTINGS_FILE), "Ctrl+Shift+F2").unwrap();
        assert_eq!(Manager::new(root.path()).shortcut, "Ctrl+Shift+F2");
    }
}
