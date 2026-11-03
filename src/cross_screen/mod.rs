mod boundary;
mod boundary_map;
mod channel;
mod control;
mod diagnostic;
pub mod hub;
pub mod link_warmth;
mod live;
pub mod pair_switch;
mod logical_boundary;
mod logical_boundary_store;
mod logical_boundary_sync;
pub mod native_input;
mod product;
mod runtime;
mod state;
pub(crate) use channel::DragMessage;
pub(crate) use diagnostic::ThreadDpiAwarenessGuard;

/// Returns the primary display's real mode dimensions without DPI virtualization.
///
/// UI threads are not guaranteed to run in a per-monitor-aware DPI context, so querying
/// `GetSystemMetrics` there can return logical pixels (for example 1792x1120 for a
/// 2240x1400 display at 125%). The display-mode query used here is context independent.
pub(crate) fn current_physical_primary_screen_size() -> std::io::Result<PhysicalScreenSize> {
    channel::PhysicalPrimaryScreen::current().map(channel::PhysicalPrimaryScreen::size)
}

/// Returns the primary display's reported physical dimensions in millimeters.
///
/// Windows display drivers may omit these values. Callers must treat an error as missing screen
/// metadata instead of inventing a physical size for spatial projection.
pub(crate) fn current_primary_screen_millimeters() -> std::io::Result<(u32, u32)> {
    use windows::Win32::Graphics::Gdi::{GetDC, GetDeviceCaps, HORZSIZE, ReleaseDC, VERTSIZE};

    let display = unsafe { GetDC(None) };
    if display.is_invalid() {
        return Err(std::io::Error::last_os_error());
    }
    let width = unsafe { GetDeviceCaps(Some(display), HORZSIZE) };
    let height = unsafe { GetDeviceCaps(Some(display), VERTSIZE) };
    unsafe {
        ReleaseDC(None, display);
    }
    let width = u32::try_from(width)
        .map_err(|_| std::io::Error::other("primary display physical width is unavailable"))?;
    let height = u32::try_from(height)
        .map_err(|_| std::io::Error::other("primary display physical height is unavailable"))?;
    if width == 0 || height == 0 {
        return Err(std::io::Error::other(
            "primary display physical dimensions are unavailable",
        ));
    }
    Ok((width, height))
}

pub use boundary::{
    BoundaryControlState, BoundaryDecision, BoundaryDiagnosticReport, BoundaryPolicy,
    BoundaryTransition, ControlSurface, PeerPlacement, PhysicalPoint, PhysicalScreenSize,
    PointerSample, ScreenBoundary, StrictBoundaryEngine, map_normalized_boundary_position,
    run_boundary_diagnostic,
};
pub use boundary_map::{
    BOUNDARY_MAP_SCHEMA_VERSION, BoundaryCorner, BoundaryHit, BoundaryMapConstraints,
    BoundaryMapV2, BoundarySegment, DEFAULT_MINIMUM_SEGMENT_PIXELS, MAX_BOUNDARY_PEERS,
    MAX_BOUNDARY_SEGMENTS, NORMALIZED_EDGE_LENGTH, PointerMotion, PreparedBoundaryMap,
};
pub use channel::{
    ApplicationProtocol, ChannelControlEvent, ChannelDiagnosticOptions, ChannelDiagnosticReport,
    ChannelDiagnosticRole, ChannelEvent, ChannelInputEvent, ChannelRole, ChannelSnapshot,
    ChannelState, ControlTransitionAck, ControlTransitionRequest, CrossScreenChannel,
    LayoutConfiguration, NormalizedMousePosition, RemoteInputEvent, ScreenMillimeterSize,
    run_channel_diagnostic,
};
pub use control::{
    ControlCoordinator, ControlProtocolAction, ControlProtocolDiagnosticReport,
    ControlProtocolState, dispatch_control_actions, run_control_protocol_diagnostic,
};
pub use diagnostic::{
    DiagnosticDpiMode, DiagnosticStopReason, InputDiagnosticOptions, InputDiagnosticReport,
    run_input_diagnostic,
};
pub(crate) use live::foreground_covers_primary_screen;
pub use live::{
    LiveControlDiagnosticOptions, LiveControlDiagnosticReport, LiveControlStopReason,
    run_live_control_diagnostic,
};
pub use logical_boundary::{
    BoundaryArcSegment, BoundaryLayoutV3, BoundaryPortal, BoundaryScreen, CompiledBoundaryLayout,
    CompiledLogicalBoundary, CompiledPortal, LOGICAL_BOUNDARY_SCHEMA_VERSION, LogicalBoundary,
    MAX_LOGICAL_BOUNDARIES, MAX_PORTAL_SEGMENTS, MAX_WORKSPACE_SCREENS, NORMALIZED_PORTAL_LENGTH,
    PortalPoint, PortalTransfer,
};
pub use logical_boundary_store::{
    AppliedBoundaryLayoutV3, BoundaryWorkspaceV3, LOGICAL_BOUNDARY_CORNER_GUARD_PIXELS,
    LOGICAL_BOUNDARY_WORKSPACE_SCHEMA_VERSION, SynchronizedScreenCatalog, SynchronizedScreenInfo,
};
pub use logical_boundary_sync::{
    BoundarySyncPhase, BoundarySyncSurface, RemoteBoundaryWorkspace, accept_remote_preview,
    decode_boundary_sync, encode_boundary_sync, encode_boundary_sync_with_initiator,
    initialize_from_remote_preview, merge_matching_confirmations, remote_pair_geometry_matches,
};
pub use product::{CrossScreenProductRuntime, CrossScreenProductSnapshot, CrossScreenProductState};
pub use runtime::{
    AUTO_CHANNEL_PORT, AutomaticChannelDiagnosticOptions, AutomaticChannelDiagnosticReport,
    AutomaticChannelRole, AutomaticControlSnapshot, CrossScreenRuntime, CrossScreenRuntimeSnapshot,
    CrossScreenRuntimeState, run_automatic_channel_diagnostic,
};
pub use state::{
    InputEvent, InputEventStats, InputLedger, InputStateMachine, KeyTransition, LedgerResult,
    MouseButton, PhysicalKey, StateDecision, WheelAxis,
};

/// Converts a pixel offset into a `MOUSEEVENTF_ABSOLUTE` coordinate that Windows maps back to the
/// same pixel. Windows recovers `floor(value * extent / 65536)`; the former truncating
/// `pixel * 65535 / (extent - 1)` landed one pixel towards the origin, so a pointer arriving at
/// y=1 below a top boundary was placed on the boundary row itself and could re-cross at once.
pub(crate) fn normalized_absolute_coordinate(offset: i32, extent: i32) -> i32 {
    if extent <= 1 {
        return 0;
    }
    let bounded = i64::from(offset.clamp(0, extent - 1));
    let extent = i64::from(extent);
    i32::try_from(((bounded * 65_536) + extent - 1) / extent)
        .unwrap_or(65_535)
        .min(65_535)
}

static OLE_DRAG_LOOPS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Marks a Pierdesk-owned OLE `DoDragDrop` loop as running on this computer for its lifetime.
///
/// That modal loop only re-evaluates `QueryContinueDrag` when it processes a mouse message. While
/// it runs, injected moves stay uncoalesced so each one wakes it; without that, a stationary
/// confined cursor produced no message and suspending the loop took ~730 ms instead of ~35 ms
/// (2026-10-11, 1.0.21). Outside such a loop moves coalesce like a physical mouse (CF-CS-014).
pub(crate) struct OleDragLoopGuard(());

impl OleDragLoopGuard {
    pub(crate) fn enter() -> Self {
        OLE_DRAG_LOOPS.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Self(())
    }
}

impl Drop for OleDragLoopGuard {
    fn drop(&mut self) {
        OLE_DRAG_LOOPS.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

pub(crate) fn ole_drag_loop_active() -> bool {
    OLE_DRAG_LOOPS.load(std::sync::atomic::Ordering::Acquire) != 0
}
