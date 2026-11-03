//! Snapshot window order before the overlay appears. UI Automation runs on a
//! dedicated MTA thread: slow or broken providers never block selection/input.
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{
    DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation8, IUIAutomation2, TreeScope, TreeScope_Children, TreeScope_Element,
    UIA_BoundingRectanglePropertyId,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
};
use windows_core::BOOL;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(super) struct Region {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Region {
    pub fn contains(self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }

    fn clipped(rect: RECT, bounds: RECT) -> Option<Self> {
        let left = rect.left.max(bounds.left);
        let top = rect.top.max(bounds.top);
        let right = rect.right.min(bounds.right);
        let bottom = rect.bottom.min(bounds.bottom);
        if right <= left || bottom <= top {
            return None;
        }
        (right - left >= 24 && bottom - top >= 24).then_some(Self {
            x: left - bounds.left,
            y: top - bounds.top,
            w: right - left,
            h: bottom - top,
        })
    }
}

#[derive(Clone, Copy, Serialize)]
pub(super) struct Target {
    #[serde(skip)]
    handle: isize,
    #[serde(flatten)]
    pub region: Region,
}

pub(super) fn snapshot(bounds: RECT) -> Vec<Target> {
    struct Context {
        bounds: RECT,
        targets: Vec<Target>,
    }
    unsafe extern "system" fn visit(window: HWND, data: LPARAM) -> BOOL {
        let context = unsafe { &mut *(data.0 as *mut Context) };
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(window, Some(&raw mut pid)) };
        if pid == std::process::id()
            || !unsafe { IsWindowVisible(window) }.as_bool()
            || unsafe { IsIconic(window) }.as_bool()
        {
            return BOOL(1);
        }
        let mut cloaked = 0u32;
        let _ =
            unsafe { DwmGetWindowAttribute(window, DWMWA_CLOAKED, (&raw mut cloaked).cast(), 4) };
        if cloaked != 0 {
            return BOOL(1);
        }
        let mut rect = RECT::default();
        if unsafe {
            DwmGetWindowAttribute(
                window,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                (&raw mut rect).cast(),
                u32::try_from(size_of::<RECT>()).unwrap_or(16),
            )
        }
        .is_err()
        {
            let _ = unsafe { GetWindowRect(window, &raw mut rect) };
        }
        if let Some(region) = Region::clipped(rect, context.bounds) {
            context.targets.push(Target {
                handle: window.0 as isize,
                region,
            });
        }
        BOOL(i32::from(context.targets.len() < 512))
    }
    let mut context = Context {
        bounds,
        targets: Vec::new(),
    };
    let _ = unsafe { EnumWindows(Some(visit), LPARAM((&raw mut context) as isize)) };
    context.targets
}

pub(super) struct Query {
    pub pixels: Option<Arc<Vec<u8>>>,
    pub id: u32,
    pub x: i32,
    pub y: i32,
    pub target: Target,
    pub bounds: RECT,
}

pub(super) struct Reply {
    pub id: u32,
    pub regions: Vec<Region>,
    pub window: Region,
    pub visual: Option<Vec<Region>>,
}

pub(super) struct Detector {
    requests: SyncSender<()>,
    latest: Arc<Mutex<Option<Query>>>,
    replies: Arc<Mutex<Option<Reply>>>,
}

impl Detector {
    pub fn new() -> Self {
        let (requests, incoming) = mpsc::sync_channel::<()>(1);
        let replies = Arc::new(Mutex::new(None));
        let outgoing = Arc::clone(&replies);
        let latest = Arc::new(Mutex::new(None::<Query>));
        let pending = Arc::clone(&latest);
        let _ = std::thread::Builder::new()
            .name("screenshot-regions".into())
            .spawn(move || {
                if unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_err() {
                    return;
                }
                {
                    let automation: windows_core::Result<IUIAutomation2> =
                        unsafe { CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER) };
                    if let Ok(automation) = automation {
                        let mut visual_cache = Vec::new();
                        unsafe {
                            let _ = automation.SetAutoSetFocus(false);
                            let _ = automation.SetConnectionTimeout(100);
                            let _ = automation.SetTransactionTimeout(100);
                        }
                        while incoming.recv().is_ok() {
                            let Some(query) = pending.lock().ok().and_then(|mut slot| slot.take())
                            else {
                                continue;
                            };
                            let mut regions = detect(&automation, &query);
                            if let Ok(mut reply) = outgoing.lock() {
                                *reply = Some(Reply {
                                    id: query.id,
                                    regions: regions.clone(),
                                    window: query.target.region,
                                    visual: None,
                                });
                            }
                            if let Some(pixels) = &query.pixels {
                                let key = (query.target.handle, query.target.region);
                                let found =
                                    visual_cache.iter().position(|(cached, _)| *cached == key);
                                let extra = if let Some(index) = found {
                                    &visual_cache[index].1
                                } else {
                                    let extra = super::visual::detect(
                                        pixels,
                                        query.bounds,
                                        query.target.region,
                                        |pictures| {
                                            let mut early = regions.clone();
                                            early.extend(
                                                pictures
                                                    .iter()
                                                    .copied()
                                                    .filter(|r| r.contains(query.x, query.y)),
                                            );
                                            early.sort_by_key(|r| i64::from(r.w) * i64::from(r.h));
                                            early.dedup();
                                            if let Ok(mut reply) = outgoing.lock() {
                                                *reply = Some(Reply {
                                                    id: query.id,
                                                    regions: early,
                                                    window: query.target.region,
                                                    visual: Some(pictures.to_vec()),
                                                });
                                            }
                                        },
                                    );
                                    if visual_cache.len() >= 4 {
                                        visual_cache.remove(0);
                                    }
                                    visual_cache.push((key, extra));
                                    &visual_cache.last().expect("just cached").1
                                };
                                regions.extend(
                                    extra
                                        .iter()
                                        .copied()
                                        .filter(|r| r.contains(query.x, query.y)),
                                );
                                regions.sort_by_key(|r| i64::from(r.w) * i64::from(r.h));
                                regions.dedup();
                                if let Ok(mut reply) = outgoing.lock() {
                                    *reply = Some(Reply {
                                        id: query.id,
                                        regions,
                                        window: query.target.region,
                                        visual: Some(extra.clone()),
                                    });
                                }
                            }
                        }
                    }
                }
                unsafe { CoUninitialize() };
            });
        Self {
            requests,
            latest,
            replies,
        }
    }

    pub fn query(&self, query: Query) {
        if let Ok(mut latest) = self.latest.lock() {
            *latest = Some(query);
        }
        let _ = self.requests.try_send(());
    }
    pub fn poll(&self) -> Option<Reply> {
        self.replies.lock().ok().and_then(|mut reply| reply.take())
    }
}

fn detect(automation: &IUIAutomation2, query: &Query) -> Vec<Region> {
    let mut regions = vec![query.target.region];
    let result = (|| -> windows_core::Result<()> {
        let hwnd = HWND(query.target.handle as *mut _);
        let request = unsafe { automation.CreateCacheRequest() }?;
        unsafe {
            request.SetTreeScope(TreeScope(TreeScope_Element.0 | TreeScope_Children.0))?;
            request.SetTreeFilter(&automation.ControlViewCondition()?)?;
            request.AddProperty(UIA_BoundingRectanglePropertyId)?;
        }
        let root = unsafe { automation.ElementFromHandleBuildCache(hwnd, &request) }?;
        let deadline = Instant::now() + Duration::from_millis(250);
        let frame = query.target.region;
        let clip = RECT {
            left: query.bounds.left + frame.x,
            top: query.bounds.top + frame.y,
            right: query.bounds.left + frame.x + frame.w,
            bottom: query.bounds.top + frame.y + frame.h,
        };
        let mut stack = vec![(root, 0)];
        let mut visited = 0;
        while let Some((element, depth)) = stack.pop() {
            if depth >= 24 || visited >= 512 || Instant::now() >= deadline {
                break;
            }
            let Ok(children) = (unsafe { element.GetCachedChildren() }) else {
                continue;
            };
            let count = unsafe { children.Length() }?.min(512);
            for index in 0..count {
                visited += 1;
                let child = unsafe { children.GetElement(index) }?;
                let rect = unsafe { child.CachedBoundingRectangle() }?;
                let Some(mut region) = Region::clipped(rect, clip) else {
                    continue;
                };
                region.x += frame.x;
                region.y += frame.y;
                if !region.contains(query.x, query.y) {
                    continue;
                }
                if !regions.contains(&region) {
                    regions.push(region);
                }
                // Keep all hit branches: layout-only panes can overlap and
                // conceal an earlier content sibling. Cached fields avoid a
                // round trip per child's property.
                if Instant::now() < deadline
                    && stack.len() < 64
                    && let Ok(cached) = unsafe { child.BuildUpdatedCache(&request) }
                {
                    stack.push((cached, depth + 1));
                }
            }
        }
        Ok(())
    })();
    let _ = result; // Unsupported providers fall back to the captured window.
    regions.sort_by_key(|region| i64::from(region.w) * i64::from(region.h));
    regions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_uia_finds_list_inside_offscreen_fixture_without_user_input() {
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, DispatchMessageW, MSG, PM_REMOVE, PeekMessageW,
            WINDOW_EX_STYLE, WS_CHILD, WS_EX_NOACTIVATE, WS_POPUP, WS_VISIBLE,
        };
        use windows_core::w;

        struct Fixture(HWND);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = unsafe { DestroyWindow(self.0) };
            }
        }
        let root = Fixture(
            unsafe {
                CreateWindowExW(
                    WS_EX_NOACTIVATE,
                    w!("#32770"),
                    w!(""),
                    WS_POPUP | WS_VISIBLE,
                    -30000,
                    -30000,
                    400,
                    300,
                    None,
                    None,
                    None,
                    None,
                )
            }
            .unwrap(),
        );
        let _list = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("LISTBOX"),
                w!(""),
                WS_CHILD | WS_VISIBLE,
                20,
                20,
                300,
                200,
                Some(root.0),
                None,
                None,
                None,
            )
        }
        .unwrap();
        let detector = Detector::new();
        detector.query(Query {
            pixels: None,
            id: 7,
            x: 50,
            y: 50,
            target: Target {
                handle: root.0.0 as isize,
                region: Region {
                    x: 0,
                    y: 0,
                    w: 400,
                    h: 300,
                },
            },
            bounds: RECT {
                left: -30000,
                top: -30000,
                right: -29600,
                bottom: -29700,
            },
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let reply = loop {
            if let Some(reply) = detector.poll() {
                break reply;
            }
            assert!(Instant::now() < deadline, "UIA worker did not reply");
            let mut message = MSG::default();
            while unsafe { PeekMessageW(&raw mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                unsafe { DispatchMessageW(&raw const message) };
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(reply.id, 7);
        assert!(
            reply.regions.iter().any(|region| region.x == 20
                && region.y == 20
                && region.w == 300
                && region.h > 0
                && region.h <= 200),
            "Missing list candidate: {:?}",
            reply.regions
        );
    }

    #[test]
    fn candidates_use_physical_pixels_with_negative_desktop_origin() {
        let bounds = RECT {
            left: -1920,
            top: -200,
            right: 2560,
            bottom: 1440,
        };
        let region = Region::clipped(
            RECT {
                left: -2000,
                top: -100,
                right: -800,
                bottom: 700,
            },
            bounds,
        )
        .unwrap();
        assert_eq!(
            region,
            Region {
                x: 0,
                y: 100,
                w: 1120,
                h: 800
            }
        );
        assert!(region.contains(0, 100));
        assert!(!region.contains(1120, 100));
        assert!(
            Region::clipped(
                RECT {
                    left: 3000,
                    top: 0,
                    right: 4000,
                    bottom: 100
                },
                bounds
            )
            .is_none()
        );
    }
}
