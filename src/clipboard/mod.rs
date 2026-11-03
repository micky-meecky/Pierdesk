mod data_object;
mod drag_helper;
mod drag_image;
pub(crate) mod drag_live;
mod drag_probe;
mod file_relay;
mod format_enum;
mod local_file;
mod loopback;
pub(crate) mod paste_fence;
mod probe;
mod product;
mod runtime;
pub mod secure_transfer;
mod shared_data_object;
mod shared_diagnostics;
mod shared_protocol;
mod snapshot;
mod source;
mod stream;
mod transfer;
mod transfer_speed;

pub use drag_helper::{launch_drag_edge_helper, run_drag_edge_helper, run_drag_helper_self_test};
pub(crate) use drag_image::{
    capture_icon as capture_shell_icon, capture_type_icon as capture_shell_type_icon,
};
pub use drag_probe::{
    DragProbeEdge, RemoteTargetDragProbeOptions, SourceDragProbeOptions, SourceDragProbeReport,
    TargetDragProbeOptions, TargetDragProbeReport, run_remote_target_drag_probe,
    run_source_drag_probe, run_target_drag_probe,
};
pub use probe::ProbeState;
pub(crate) use product::{
    BOUNDARY_WORKSPACE_ACK, BOUNDARY_WORKSPACE_MAGIC, BOUNDARY_WORKSPACE_PAYLOAD_LIMIT,
};
pub use product::{
    ProductCommand, ProductFileSnapshot, ProductManifestSummary, ProductRuntime,
    ProductSharedTextSummary, ProductSnapshot, ProductTransferDirection, ProductTransferId,
    ProductTransferSnapshot, ProductTransferState, ProductTransferTaskSnapshot,
};
pub use runtime::{
    ClipboardProbeOptions, FileCaptureProbeOptions, LoopbackProbeOptions, PauseProbeOptions,
    SecureFetchProbeOptions, SecureFetchResult, SecureReceiverProbeOptions,
    SecureSourceProbeOptions, SecureSourceTls, run_clipboard_probe, run_file_capture_probe,
    run_loopback_probe, run_pause_probe, run_secure_fetch_probe, run_secure_receiver_probe,
    run_secure_source_probe,
};
pub(crate) use shared_data_object::png_to_dibv5;
pub(crate) use snapshot::normalize_png;
pub use snapshot::run_shared_clipboard_capture_helper;

pub const TEST_FILE_NAME: &str = "RemoteClipboard-Test.txt";
pub const TEST_FILE_CONTENT: &[u8] = b"Pierdesk virtual file stream test.\r\n";

pub(crate) fn catch_com_result<T>(
    operation: impl FnOnce() -> windows::core::Result<T>,
) -> windows::core::Result<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation)).unwrap_or_else(|_| {
        Err(windows::core::Error::from_hresult(
            windows::Win32::Foundation::E_UNEXPECTED,
        ))
    })
}

pub(crate) fn catch_com_hresult(
    operation: impl FnOnce() -> windows::core::HRESULT,
) -> windows::core::HRESULT {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation))
        .unwrap_or(windows::Win32::Foundation::E_UNEXPECTED)
}
