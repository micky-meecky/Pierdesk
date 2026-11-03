use std::collections::VecDeque;
use std::io::{self, Cursor, Read, Write};
use std::mem::size_of;
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::windows::io::AsRawSocket;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rustls::{Connection, StreamOwned};
use windows::Win32::Foundation::{HANDLE, POINT};
use windows::Win32::Graphics::Gdi::{DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW};
use windows::Win32::NetworkManagement::QoS::{
    QOS_NON_ADAPTIVE_FLOW, QOS_VERSION, QOSAddSocketToFlow, QOSCloseHandle, QOSCreateHandle,
    QOSRemoveSocketFromFlow, QOSTrafficTypeControl,
};
use windows::Win32::Networking::WinSock::{
    FIONREAD, SIO_TCP_INFO, SOCKET, TCP_INFO_v0, WSAIoctl, ioctlsocket,
};
use windows::Win32::UI::WindowsAndMessaging::GetPhysicalCursorPos;
use windows_core::PCWSTR;

use crate::device_store::DeviceStore;
use crate::security::{CertificateFingerprint, PinnedTlsClient, PinnedTlsServer, TrustedTlsServer};

use super::boundary::{PeerPlacement, PhysicalScreenSize, ScreenBoundary};
use super::state::{KeyTransition, MouseButton, PhysicalKey, WheelAxis};

const FRAME_MAGIC: &[u8; 8] = b"CFCSCH16";
const FRAME_VERSION: u16 = 16;
const FRAME_HEADER_LENGTH: usize = 80;
const MAX_FRAME_PAYLOAD: usize = 60 * 1024;
const MAX_DECODE_BUFFER: usize = 256 * 1024;
const MAX_APPLICATION_PAYLOAD: usize = 16 * 1024;
const APPLICATION_QUEUE_CAPACITY: usize = 8;
const CONTROL_APPLICATION_QUEUE_CAPACITY: usize = 256;
const RELIABLE_QUEUE_CAPACITY: usize = 64;
const INBOUND_CONTROL_QUEUE_CAPACITY: usize = 64;
const INBOUND_INPUT_QUEUE_CAPACITY: usize = 256;
const MAX_RECEIVED_FRAMES_PER_SECOND: u32 = 2_048;
const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(500);
// CF-CS-007: after the link carried only 500 ms heartbeats for a while, the first seconds of
// dense pointer traffic to a Wi-Fi peer lose packets and stall on TCP retransmission (1 s+ read
// gaps, rising timeout episodes) before the link recovers. Ahead of a likely crossing, ping
// densely for a few seconds so that recovery happens before the pointer arrives.
const WARMUP_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(10);
const WARMUP_DURATION: Duration = Duration::from_secs(3);
const WARMUP_COOLDOWN: Duration = Duration::from_secs(15);
// Sustained keep-warm while a user is active and both ends allow it (see `link_warmth`).
// 1.0.16 used 50 ms (~16 pings/s with RTT gating) and the link still dropped packets when real
// pointer traffic (~130 messages/s) started; keep it close to that load instead. One ping is
// outstanding at a time, so the rate is bounded by the round trip (about 100/s at 3 ms RTT).
const KEEP_WARM_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(10);
// Carried in the top bit of each ping nonce: "this end allows keep-warm". Older builds echo the
// nonce unchanged and never set it, so they simply never receive keep-warm traffic.
const PING_WARM_ALLOWED: u64 = 1 << 63;
// Three seconds was too aggressive for Windows wake-up, scheduler stalls, and brief Wi-Fi
// contention. Keep the 500 ms probe cadence but require a sustained silence before recycling the
// authenticated session.
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(10);
const READER_POLL_INTERVAL: Duration = Duration::from_millis(250);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
pub(super) const MOUSE_SAMPLE_INTERVAL: Duration = Duration::from_millis(8);
const MAX_CHANNEL_LIFETIME: Duration = Duration::from_mins(5);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelRole {
    Listener,
    Connector,
}

impl ChannelRole {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Listener => "listener",
            Self::Connector => "connector",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelState {
    Connected,
    Closing,
    Stopped,
    Failed,
}

impl ChannelState {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Closing => "closing",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NormalizedMousePosition {
    pub x: u16,
    pub y: u16,
}

/// Physical display dimensions reported by the operating system, in millimeters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScreenMillimeterSize {
    pub width: u32,
    pub height: u32,
}

impl ScreenMillimeterSize {
    /// Validates physical display dimensions before they enter the authenticated channel.
    ///
    /// # Errors
    ///
    /// Returns an error for zero or implausibly large dimensions.
    pub fn new(width: u32, height: u32) -> io::Result<Self> {
        if width == 0 || height == 0 || width > 10_000 || height > 10_000 {
            return Err(invalid_data(
                "cross-screen physical display dimensions are outside their bounds",
            ));
        }
        Ok(Self { width, height })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteInputEvent {
    Key {
        physical: PhysicalKey,
        virtual_key: u16,
        transition: KeyTransition,
    },
    MouseMove(NormalizedMousePosition),
    MouseButton {
        button: MouseButton,
        transition: KeyTransition,
    },
    Wheel {
        axis: WheelAxis,
        delta: i16,
    },
}

impl RemoteInputEvent {
    #[must_use]
    pub const fn is_mouse_move(self) -> bool {
        matches!(self, Self::MouseMove(_))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChannelInputEvent {
    pub control_epoch: u64,
    pub event: RemoteInputEvent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlTransitionRequest {
    pub control_epoch: u64,
    pub departure_boundary: ScreenBoundary,
    pub normalized_edge_offset: u16,
    pub logical_boundary: Option<LogicalBoundaryTransition>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogicalBoundaryTransition {
    pub boundary_id: String,
    pub mapping_revision: u64,
    pub parameter: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControlTransitionAck {
    pub control_epoch: u64,
    pub accepted: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LayoutConfiguration {
    pub listener_peer_placement: PeerPlacement,
    pub revision: u64,
    pub corner_guard_enabled: bool,
    pub corner_guard_pixels: u8,
    pub require_second_push: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChannelControlEvent {
    FileDrag(DragMessage),
    ScreenInfo(PhysicalScreenSize),
    ScreenMillimeters(Option<ScreenMillimeterSize>),
    LayoutInfo(LayoutConfiguration),
    BoundaryWorkspace(Vec<u8>),
    EnterRequest(ControlTransitionRequest),
    EnterAcknowledgement(ControlTransitionAck),
    ReturnRequest(ControlTransitionRequest),
    ReturnAcknowledgement(ControlTransitionAck),
    ReleaseAll,
}

/// A gesture is scoped to the authenticated channel binding; ids increase within that channel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DragMessage {
    pub id: u64,
    pub kind: u8,
    pub port: u16,
}

impl DragMessage {
    pub const PREPARE: u8 = 1;
    pub const READY: u8 = 2;
    pub const ACTIVATE: u8 = 3;
    pub const RELEASE: u8 = 4;
    pub const CANCEL: u8 = 5;
    pub const FINISHED: u8 = 6;
    pub const CANDIDATE: u8 = 7;
    /// The source offer is reachable through the listener-side loopback proxy rather than the
    /// peer address carried by the cross-screen session.
    pub const PREPARE_TUNNEL: u8 = 8;

    #[must_use]
    pub const fn is_prepare(self) -> bool {
        self.kind == Self::PREPARE || self.kind == Self::PREPARE_TUNNEL
    }

    fn encode(self) -> io::Result<Vec<u8>> {
        if self.id == 0 || !(1..=8).contains(&self.kind) || self.is_prepare() != (self.port != 0) {
            return Err(invalid_data("invalid file drag message"));
        }
        let mut payload = vec![0; 12];
        payload[..8].copy_from_slice(&self.id.to_le_bytes());
        payload[8] = self.kind;
        payload[10..12].copy_from_slice(&self.port.to_le_bytes());
        Ok(payload)
    }

    fn decode(payload: &[u8]) -> io::Result<Self> {
        if payload.len() != 12 || payload[9] != 0 {
            return Err(invalid_data("invalid file drag payload"));
        }
        let message = Self {
            id: read_u64(payload, 0)?,
            kind: payload[8],
            port: read_u16(payload, 10)?,
        };
        message.encode()?;
        Ok(message)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChannelEvent {
    Control(ChannelControlEvent),
    Input(ChannelInputEvent),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChannelSnapshot {
    pub role: ChannelRole,
    pub state: ChannelState,
    pub local_fingerprint: CertificateFingerprint,
    pub peer_fingerprint: CertificateFingerprint,
    pub local_address: SocketAddr,
    pub peer_address: SocketAddr,
    pub session_id: String,
    /// Random nonce that binds every frame to this exact TLS connection.
    pub channel_epoch: u64,
    pub sent_frames: u64,
    pub received_frames: u64,
    pub sent_mouse_moves: u64,
    pub received_mouse_moves: u64,
    pub coalesced_mouse_moves: u64,
    pub sent_heartbeats: u64,
    pub received_heartbeats: u64,
    pub sent_release_all: u64,
    pub received_release_all: u64,
    pub sent_control_messages: u64,
    pub received_control_messages: u64,
    pub sent_input_events: u64,
    pub received_input_events: u64,
    pub coalesced_input_mouse_moves: u64,
    pub last_sent_sequence: u64,
    pub last_received_sequence: u64,
    pub round_trip_micros: Option<u64>,
    pub latency_qos_enabled: bool,
    pub latency_qos_error: Option<String>,
    pub clean_shutdown: bool,
    pub last_error: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct InputCadenceLaneSnapshot {
    pub moves: u64,
    pub max_gap_micros: u64,
    pub gaps_over_12_ms: u64,
    pub gaps_over_20_ms: u64,
    pub gaps_over_33_ms: u64,
    pub gaps_over_50_ms: u64,
    pub gaps_over_100_ms: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct InputCadenceSnapshot {
    pub enqueued: InputCadenceLaneSnapshot,
    pub sent: InputCadenceLaneSnapshot,
    pub received: InputCadenceLaneSnapshot,
}

#[derive(Default)]
struct InputCadenceLane {
    last_micros: AtomicU64,
    moves: AtomicU64,
    max_gap_micros: AtomicU64,
    gaps_over_12_ms: AtomicU64,
    gaps_over_20_ms: AtomicU64,
    gaps_over_33_ms: AtomicU64,
    gaps_over_50_ms: AtomicU64,
    gaps_over_100_ms: AtomicU64,
}

impl InputCadenceLane {
    fn observe(&self) {
        let now = input_timing_micros();
        let previous = self.last_micros.swap(now, Ordering::Relaxed);
        self.moves.fetch_add(1, Ordering::Relaxed);
        if previous == 0 {
            return;
        }
        let gap = now.saturating_sub(previous);
        self.max_gap_micros.fetch_max(gap, Ordering::Relaxed);
        if gap > 12_000 {
            self.gaps_over_12_ms.fetch_add(1, Ordering::Relaxed);
        }
        if gap > 20_000 {
            self.gaps_over_20_ms.fetch_add(1, Ordering::Relaxed);
        }
        if gap > 33_000 {
            self.gaps_over_33_ms.fetch_add(1, Ordering::Relaxed);
        }
        if gap > 50_000 {
            self.gaps_over_50_ms.fetch_add(1, Ordering::Relaxed);
        }
        if gap > 100_000 {
            self.gaps_over_100_ms.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn take(&self) -> InputCadenceLaneSnapshot {
        InputCadenceLaneSnapshot {
            moves: self.moves.swap(0, Ordering::Relaxed),
            max_gap_micros: self.max_gap_micros.swap(0, Ordering::Relaxed),
            gaps_over_12_ms: self.gaps_over_12_ms.swap(0, Ordering::Relaxed),
            gaps_over_20_ms: self.gaps_over_20_ms.swap(0, Ordering::Relaxed),
            gaps_over_33_ms: self.gaps_over_33_ms.swap(0, Ordering::Relaxed),
            gaps_over_50_ms: self.gaps_over_50_ms.swap(0, Ordering::Relaxed),
            gaps_over_100_ms: self.gaps_over_100_ms.swap(0, Ordering::Relaxed),
        }
    }

    fn reset(&self) {
        let _ = self.take();
        self.last_micros.store(0, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct InputCadenceProbe {
    enqueued: InputCadenceLane,
    sent: InputCadenceLane,
    received: InputCadenceLane,
}

impl InputCadenceProbe {
    fn take(&self) -> InputCadenceSnapshot {
        InputCadenceSnapshot {
            enqueued: self.enqueued.take(),
            sent: self.sent.take(),
            received: self.received.take(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ControlCadenceSnapshot {
    pub enqueued: InputCadenceLaneSnapshot,
    pub sent: InputCadenceLaneSnapshot,
    pub received: InputCadenceLaneSnapshot,
    pub dequeued: InputCadenceLaneSnapshot,
}

#[derive(Default)]
struct ControlCadenceProbe {
    enqueued: InputCadenceLane,
    sent: InputCadenceLane,
    received: InputCadenceLane,
    dequeued: InputCadenceLane,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ReaderStageSnapshot {
    pub socket_reads: u64,
    pub socket_timeouts: u64,
    pub socket_bytes: u64,
    pub socket_wait_max_us: u64,
    pub outside_read_max_us: u64,
    pub tls_max_us: u64,
    pub dispatch_max_us: u64,
}

#[derive(Default)]
struct ReaderStageProbe {
    last_loop_end_micros: AtomicU64,
    socket_reads: AtomicU64,
    socket_timeouts: AtomicU64,
    socket_bytes: AtomicU64,
    socket_wait_max_us: AtomicU64,
    outside_read_max_us: AtomicU64,
    tls_max_us: AtomicU64,
    dispatch_max_us: AtomicU64,
}

impl ReaderStageProbe {
    fn begin_iteration(&self) {
        let previous = self.last_loop_end_micros.load(Ordering::Relaxed);
        if previous != 0 {
            let gap = input_timing_micros().saturating_sub(previous);
            self.outside_read_max_us.fetch_max(gap, Ordering::Relaxed);
        }
    }

    fn end_iteration(&self) {
        self.last_loop_end_micros
            .store(input_timing_micros(), Ordering::Relaxed);
    }

    fn socket_read(&self, elapsed: Duration, bytes: usize, timed_out: bool) {
        self.socket_reads.fetch_add(1, Ordering::Relaxed);
        self.socket_bytes
            .fetch_add(u64::try_from(bytes).unwrap_or(u64::MAX), Ordering::Relaxed);
        if timed_out {
            self.socket_timeouts.fetch_add(1, Ordering::Relaxed);
        }
        self.socket_wait_max_us
            .fetch_max(duration_us(elapsed), Ordering::Relaxed);
    }

    fn tls_finished(&self, elapsed: Duration) {
        self.tls_max_us
            .fetch_max(duration_us(elapsed), Ordering::Relaxed);
    }

    fn dispatch_finished(&self, elapsed: Duration) {
        self.dispatch_max_us
            .fetch_max(duration_us(elapsed), Ordering::Relaxed);
    }

    fn take(&self) -> ReaderStageSnapshot {
        ReaderStageSnapshot {
            socket_reads: self.socket_reads.swap(0, Ordering::Relaxed),
            socket_timeouts: self.socket_timeouts.swap(0, Ordering::Relaxed),
            socket_bytes: self.socket_bytes.swap(0, Ordering::Relaxed),
            socket_wait_max_us: self.socket_wait_max_us.swap(0, Ordering::Relaxed),
            outside_read_max_us: self.outside_read_max_us.swap(0, Ordering::Relaxed),
            tls_max_us: self.tls_max_us.swap(0, Ordering::Relaxed),
            dispatch_max_us: self.dispatch_max_us.swap(0, Ordering::Relaxed),
        }
    }

    fn reset(&self) {
        let _ = self.take();
        self.last_loop_end_micros.store(0, Ordering::Relaxed);
    }
}

fn duration_us(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TcpPathSnapshot {
    pub bytes_out: u64,
    pub bytes_in: u64,
    pub bytes_retrans: u32,
    pub timeout_episodes: u32,
    pub rtt_us: u32,
    pub bytes_in_flight: u32,
    pub receive_window: u32,
    pub pending_bytes: Option<u32>,
}

fn tcp_path_snapshot(socket: &TcpStream) -> io::Result<TcpPathSnapshot> {
    let raw = SOCKET(
        usize::try_from(socket.as_raw_socket())
            .map_err(|_| io::Error::other("socket handle exceeds pointer width"))?,
    );
    let version = 0_u32;
    let mut info = TCP_INFO_v0::default();
    let mut returned = 0_u32;
    // SIO_TCP_INFO is a read-only socket query; unlike TCP EStats it does not
    // enable collection or change the connection's behavior.
    let result = unsafe {
        WSAIoctl(
            raw,
            SIO_TCP_INFO,
            Some((&raw const version).cast()),
            u32::try_from(size_of::<u32>()).expect("DWORD fits in u32"),
            Some((&raw mut info).cast()),
            u32::try_from(size_of::<TCP_INFO_v0>()).expect("TCP_INFO_v0 fits in u32"),
            &raw mut returned,
            None,
            None,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if usize::try_from(returned).unwrap_or(0) < size_of::<TCP_INFO_v0>() {
        return Err(io::Error::other("short SIO_TCP_INFO response"));
    }
    let mut pending = 0_u32;
    let pending_bytes =
        (unsafe { ioctlsocket(raw, FIONREAD, &raw mut pending) } == 0).then_some(pending);
    Ok(TcpPathSnapshot {
        bytes_out: info.BytesOut,
        bytes_in: info.BytesIn,
        bytes_retrans: info.BytesRetrans,
        timeout_episodes: info.TimeoutEpisodes,
        rtt_us: info.RttUs,
        bytes_in_flight: info.BytesInFlight,
        receive_window: info.RcvWnd,
        pending_bytes,
    })
}

impl ControlCadenceProbe {
    fn reset(&self) {
        self.enqueued.reset();
        self.sent.reset();
        self.received.reset();
        self.dequeued.reset();
    }

    fn take(&self) -> ControlCadenceSnapshot {
        ControlCadenceSnapshot {
            enqueued: self.enqueued.take(),
            sent: self.sent.take(),
            received: self.received.take(),
            dequeued: self.dequeued.take(),
        }
    }
}

fn input_timing_micros() -> u64 {
    static STARTED: OnceLock<Instant> = OnceLock::new();
    u64::try_from(STARTED.get_or_init(Instant::now).elapsed().as_micros())
        .unwrap_or(u64::MAX)
        .saturating_add(1)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelDiagnosticRole {
    Listen(SocketAddr),
    Connect(SocketAddr),
}

impl ChannelDiagnosticRole {
    #[must_use]
    pub const fn role(&self) -> ChannelRole {
        match self {
            Self::Listen(_) => ChannelRole::Listener,
            Self::Connect(_) => ChannelRole::Connector,
        }
    }

    #[must_use]
    pub const fn address(&self) -> SocketAddr {
        match self {
            Self::Listen(address) | Self::Connect(address) => *address,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ChannelDiagnosticOptions {
    pub store: DeviceStore,
    pub peer_fingerprint: CertificateFingerprint,
    pub role: ChannelDiagnosticRole,
    pub connect_timeout: Duration,
    pub lifetime: Duration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChannelDiagnosticReport {
    pub sampled_mouse_positions: u64,
    pub snapshot: ChannelSnapshot,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum FrameKind {
    Hello = 1,
    HelloAcknowledgement = 2,
    MouseMove = 3,
    Ping = 4,
    Pong = 5,
    ReleaseAll = 6,
    Close = 7,
    ScreenInfo = 8,
    EnterRequest = 9,
    EnterAcknowledgement = 10,
    ReturnRequest = 11,
    ReturnAcknowledgement = 12,
    InputKey = 13,
    InputMouseMove = 14,
    InputMouseButton = 15,
    InputWheel = 16,
    LayoutInfo = 17,
    FileDrag = 18,
    ScreenMillimeters = 19,
    BoundaryWorkspace = 20,
    Application = 21,
}

impl FrameKind {
    fn from_byte(value: u8) -> io::Result<Self> {
        match value {
            1 => Ok(Self::Hello),
            2 => Ok(Self::HelloAcknowledgement),
            3 => Ok(Self::MouseMove),
            4 => Ok(Self::Ping),
            5 => Ok(Self::Pong),
            6 => Ok(Self::ReleaseAll),
            7 => Ok(Self::Close),
            8 => Ok(Self::ScreenInfo),
            9 => Ok(Self::EnterRequest),
            10 => Ok(Self::EnterAcknowledgement),
            11 => Ok(Self::ReturnRequest),
            12 => Ok(Self::ReturnAcknowledgement),
            13 => Ok(Self::InputKey),
            14 => Ok(Self::InputMouseMove),
            15 => Ok(Self::InputMouseButton),
            16 => Ok(Self::InputWheel),
            17 => Ok(Self::LayoutInfo),
            18 => Ok(Self::FileDrag),
            19 => Ok(Self::ScreenMillimeters),
            20 => Ok(Self::BoundaryWorkspace),
            21 => Ok(Self::Application),
            _ => Err(invalid_data("unknown cross-screen frame kind")),
        }
    }

    const fn expected_payload_length(self) -> Option<usize> {
        match self {
            Self::Hello | Self::HelloAcknowledgement | Self::ReleaseAll | Self::Close => Some(0),
            Self::MouseMove => Some(4),
            Self::Ping | Self::Pong | Self::ScreenInfo | Self::ScreenMillimeters => Some(8),
            Self::EnterAcknowledgement
            | Self::ReturnAcknowledgement
            | Self::InputMouseMove
            | Self::FileDrag => Some(12),
            Self::EnterRequest
            | Self::ReturnRequest
            | Self::BoundaryWorkspace
            | Self::Application => None,
            Self::InputKey | Self::LayoutInfo => Some(16),
            Self::InputMouseButton => Some(10),
            Self::InputWheel => Some(11),
        }
    }

    const fn valid_payload_length(self, length: usize) -> bool {
        if matches!(self, Self::Application) {
            return length >= 2 && length <= MAX_APPLICATION_PAYLOAD;
        }
        if matches!(self, Self::EnterRequest | Self::ReturnRequest) {
            return length == 12 || (length >= 24 && length <= 87);
        }
        match self.expected_payload_length() {
            Some(expected) => length == expected,
            None => length > 0 && length <= MAX_FRAME_PAYLOAD,
        }
    }
}

/// Independent bounded protocol queues inside one authenticated peer connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ApplicationProtocol {
    Control = 0,
    Layout = 1,
    Clipboard = 2,
    File = 3,
    /// Per-pair cross-screen switch requests and acknowledgements (see `pair_switch`).
    PairSwitch = 4,
}

impl ApplicationProtocol {
    fn parse(tag: u8) -> io::Result<Self> {
        match tag {
            0 => Ok(Self::Control),
            1 => Ok(Self::Layout),
            2 => Ok(Self::Clipboard),
            3 => Ok(Self::File),
            4 => Ok(Self::PairSwitch),
            _ => Err(invalid_data("unknown workspace protocol")),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ApplicationMessage {
    protocol: ApplicationProtocol,
    payload: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Frame {
    kind: FrameKind,
    sender: CertificateFingerprint,
    session_id: [u8; 16],
    channel_epoch: u64,
    sequence: u64,
    payload: Vec<u8>,
}

impl Frame {
    fn encode(&self) -> io::Result<Vec<u8>> {
        if self.sequence == 0 {
            return Err(invalid_data("cross-screen frame sequence must be non-zero"));
        }
        if self.session_id.iter().all(|byte| *byte == 0) || self.channel_epoch == 0 {
            return Err(invalid_data(
                "cross-screen frame session and channel epoch must be non-zero",
            ));
        }
        if !self.kind.valid_payload_length(self.payload.len()) {
            return Err(invalid_data(
                "cross-screen frame payload length does not match its kind",
            ));
        }
        let payload_length = u16::try_from(self.payload.len())
            .map_err(|_| invalid_data("cross-screen frame payload is too large"))?;
        let mut encoded = Vec::with_capacity(FRAME_HEADER_LENGTH + self.payload.len());
        encoded.extend_from_slice(FRAME_MAGIC);
        encoded.extend_from_slice(&FRAME_VERSION.to_le_bytes());
        encoded.push(self.kind as u8);
        encoded.push(0);
        encoded.extend_from_slice(&payload_length.to_le_bytes());
        encoded.extend_from_slice(&0_u16.to_le_bytes());
        encoded.extend_from_slice(&self.sender.bytes());
        encoded.extend_from_slice(&self.session_id);
        encoded.extend_from_slice(&self.channel_epoch.to_le_bytes());
        encoded.extend_from_slice(&self.sequence.to_le_bytes());
        encoded.extend_from_slice(&self.payload);
        debug_assert_eq!(encoded.len(), FRAME_HEADER_LENGTH + self.payload.len());
        Ok(encoded)
    }

    fn decode(encoded: &[u8]) -> io::Result<Self> {
        if encoded.len() < FRAME_HEADER_LENGTH {
            return Err(invalid_data("truncated cross-screen frame header"));
        }
        if &encoded[..8] != FRAME_MAGIC {
            return Err(invalid_data("cross-screen frame magic mismatch"));
        }
        if read_u16(encoded, 8)? != FRAME_VERSION {
            return Err(invalid_data("unsupported cross-screen frame version"));
        }
        let kind = FrameKind::from_byte(encoded[10])?;
        if encoded[11] != 0 || read_u16(encoded, 14)? != 0 {
            return Err(invalid_data(
                "cross-screen frame reserved fields must be zero",
            ));
        }
        let payload_length = usize::from(read_u16(encoded, 12)?);
        if !kind.valid_payload_length(payload_length)
            || encoded.len() != FRAME_HEADER_LENGTH + payload_length
        {
            return Err(invalid_data("invalid cross-screen frame payload length"));
        }
        let mut sender = [0_u8; 32];
        sender.copy_from_slice(&encoded[16..48]);
        let mut session_id = [0_u8; 16];
        session_id.copy_from_slice(&encoded[48..64]);
        let channel_epoch = read_u64(encoded, 64)?;
        let sequence = read_u64(encoded, 72)?;
        if session_id.iter().all(|byte| *byte == 0) || channel_epoch == 0 || sequence == 0 {
            return Err(invalid_data(
                "cross-screen frame session, epoch, and sequence must be non-zero",
            ));
        }
        Ok(Self {
            kind,
            sender: CertificateFingerprint::from_bytes(sender),
            session_id,
            channel_epoch,
            sequence,
            payload: encoded[FRAME_HEADER_LENGTH..].to_vec(),
        })
    }
}

#[derive(Debug, Default)]
struct FrameDecoder {
    buffered: Vec<u8>,
}

impl FrameDecoder {
    fn push(&mut self, bytes: &[u8]) -> io::Result<Vec<Frame>> {
        if self.buffered.len().saturating_add(bytes.len()) > MAX_DECODE_BUFFER {
            return Err(invalid_data(
                "cross-screen receive buffer exceeded its fixed limit",
            ));
        }
        self.buffered.extend_from_slice(bytes);
        let mut frames = Vec::new();
        loop {
            if self.buffered.len() < FRAME_HEADER_LENGTH {
                break;
            }
            if &self.buffered[..8] != FRAME_MAGIC {
                return Err(invalid_data("cross-screen frame magic mismatch"));
            }
            let payload_length = usize::from(read_u16(&self.buffered, 12)?);
            if !FrameKind::from_byte(self.buffered[10])?.valid_payload_length(payload_length) {
                return Err(invalid_data("cross-screen frame payload exceeds its limit"));
            }
            let frame_length = FRAME_HEADER_LENGTH + payload_length;
            if self.buffered.len() < frame_length {
                break;
            }
            let frame = Frame::decode(&self.buffered[..frame_length])?;
            self.buffered.drain(..frame_length);
            frames.push(frame);
        }
        Ok(frames)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SessionBinding {
    session_id: [u8; 16],
    channel_epoch: u64,
}

impl SessionBinding {
    fn generate() -> io::Result<Self> {
        let mut session_id = [0_u8; 16];
        getrandom::fill(&mut session_id)
            .map_err(|error| io::Error::other(format!("session randomness failed: {error}")))?;
        if session_id.iter().all(|byte| *byte == 0) {
            session_id[0] = 1;
        }
        let mut epoch_bytes = [0_u8; 8];
        getrandom::fill(&mut epoch_bytes)
            .map_err(|error| io::Error::other(format!("epoch randomness failed: {error}")))?;
        let mut channel_epoch = u64::from_le_bytes(epoch_bytes);
        if channel_epoch == 0 {
            channel_epoch = 1;
        }
        Ok(Self {
            session_id,
            channel_epoch,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ReliableMessage {
    FileDrag(DragMessage),
    Pong(u64),
    ScreenInfo(PhysicalScreenSize),
    ScreenMillimeters(Option<ScreenMillimeterSize>),
    LayoutInfo(LayoutConfiguration),
    BoundaryWorkspace(Vec<u8>),
    EnterRequest(ControlTransitionRequest),
    EnterAcknowledgement(ControlTransitionAck),
    ReturnRequest(ControlTransitionRequest),
    ReturnAcknowledgement(ControlTransitionAck),
    Input(ChannelInputEvent),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum QueueItem {
    MouseMove(NormalizedMousePosition),
    Reliable(ReliableMessage),
    ReleaseAll,
    Shutdown,
    Application(ApplicationMessage),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum OutboundQueueLifecycle {
    #[default]
    Open,
    ShutdownPending,
    ShutdownDelivered,
    Closed,
}

#[derive(Debug, Default)]
struct OutboundQueueState {
    latest_mouse_move: Option<NormalizedMousePosition>,
    reliable: VecDeque<ReliableMessage>,
    release_all_requested: bool,
    heartbeat_requested: bool,
    lifecycle: OutboundQueueLifecycle,
    application: [VecDeque<Vec<u8>>; 5],
}

#[derive(Debug, Default)]
struct OutboundQueue {
    state: Mutex<OutboundQueueState>,
    ready: Condvar,
}

impl OutboundQueue {
    fn push_application(&self, protocol: ApplicationProtocol, payload: Vec<u8>) -> io::Result<()> {
        if payload.is_empty() || payload.len() >= MAX_APPLICATION_PAYLOAD {
            return Err(invalid_data("workspace message exceeds its payload bound"));
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("outbound queue poisoned"))?;
        if state.lifecycle != OutboundQueueLifecycle::Open {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "channel is closing",
            ));
        }
        let queue = &mut state.application[protocol as usize];
        if queue.len() >= application_queue_capacity(protocol) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "workspace protocol queue is full",
            ));
        }
        queue.push_back(payload);
        self.ready.notify_one();
        Ok(())
    }
    fn push_mouse_move(&self, position: NormalizedMousePosition) -> io::Result<bool> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("cross-screen outbound queue was poisoned"))?;
        if state.lifecycle != OutboundQueueLifecycle::Open {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "cross-screen channel is closing",
            ));
        }
        let coalesced = state.latest_mouse_move.replace(position).is_some();
        self.ready.notify_one();
        Ok(coalesced)
    }

    fn push_reliable(&self, message: ReliableMessage) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("cross-screen outbound queue was poisoned"))?;
        if state.lifecycle != OutboundQueueLifecycle::Open {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "cross-screen channel is closing",
            ));
        }
        if state.reliable.len() >= RELIABLE_QUEUE_CAPACITY {
            return Err(io::Error::other(
                "cross-screen reliable queue reached its fixed capacity",
            ));
        }
        state.reliable.push_back(message);
        self.ready.notify_one();
        Ok(())
    }

    fn push_input(&self, input: ChannelInputEvent) -> io::Result<bool> {
        validate_channel_input_event(input)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("cross-screen outbound queue was poisoned"))?;
        if state.lifecycle != OutboundQueueLifecycle::Open {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "cross-screen channel is closing",
            ));
        }
        if input.event.is_mouse_move()
            && let Some(ReliableMessage::Input(previous)) = state.reliable.back_mut()
            && previous.event.is_mouse_move()
            && previous.control_epoch == input.control_epoch
        {
            *previous = input;
            self.ready.notify_one();
            return Ok(true);
        }
        if state.reliable.len() >= RELIABLE_QUEUE_CAPACITY {
            return Err(io::Error::other(
                "cross-screen reliable input queue reached its fixed capacity",
            ));
        }
        state.reliable.push_back(ReliableMessage::Input(input));
        self.ready.notify_one();
        Ok(false)
    }

    fn request_heartbeat(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.lifecycle == OutboundQueueLifecycle::Open {
            state.heartbeat_requested = true;
            self.ready.notify_all();
        }
    }

    fn request_release_all(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.lifecycle == OutboundQueueLifecycle::Open {
            state.latest_mouse_move = None;
            // A pointer handoff releases input, not the independent file-drag offer.
            // Dropping a queued READY/ACTIVATE/RELEASE here leaves one side waiting forever.
            state.reliable.retain(|message| {
                matches!(
                    message,
                    ReliableMessage::FileDrag(_) | ReliableMessage::BoundaryWorkspace(_)
                )
            });
            state.application[ApplicationProtocol::Control as usize].clear();
            state.release_all_requested = true;
            self.ready.notify_one();
        }
    }

    fn request_shutdown(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.lifecycle == OutboundQueueLifecycle::Open {
            state.lifecycle = OutboundQueueLifecycle::ShutdownPending;
            state.latest_mouse_move = None;
            state.reliable.clear();
            for queue in &mut state.application {
                queue.clear();
            }
            state.release_all_requested = false;
            self.ready.notify_all();
        }
    }

    fn pop_until(&self, deadline: Instant) -> io::Result<Option<QueueItem>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("cross-screen outbound queue was poisoned"))?;
        loop {
            if state.lifecycle == OutboundQueueLifecycle::Closed {
                return Ok(None);
            }
            if state.lifecycle == OutboundQueueLifecycle::ShutdownPending {
                state.lifecycle = OutboundQueueLifecycle::ShutdownDelivered;
                return Ok(Some(QueueItem::Shutdown));
            }
            if state.release_all_requested {
                state.release_all_requested = false;
                return Ok(Some(QueueItem::ReleaseAll));
            }
            if let Some(message) = state.reliable.pop_front() {
                return Ok(Some(QueueItem::Reliable(message)));
            }
            if let Some(position) = state.latest_mouse_move.take() {
                return Ok(Some(QueueItem::MouseMove(position)));
            }
            if state.heartbeat_requested {
                // Return early so the writer re-evaluates its (now shorter) heartbeat deadline.
                state.heartbeat_requested = false;
                return Ok(None);
            }
            for protocol in [
                ApplicationProtocol::Control,
                ApplicationProtocol::Layout,
                ApplicationProtocol::Clipboard,
                ApplicationProtocol::File,
                ApplicationProtocol::PairSwitch,
            ] {
                if let Some(payload) = state.application[protocol as usize].pop_front() {
                    return Ok(Some(QueueItem::Application(ApplicationMessage {
                        protocol,
                        payload,
                    })));
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            let (next, timeout) = self
                .ready
                .wait_timeout(state, remaining)
                .map_err(|_| io::Error::other("cross-screen outbound queue was poisoned"))?;
            state = next;
            if timeout.timed_out() {
                return Ok(None);
            }
        }
    }

    fn wake(&self) {
        self.ready.notify_all();
    }

    fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.lifecycle = OutboundQueueLifecycle::Closed;
        state.latest_mouse_move = None;
        state.reliable.clear();
        for queue in &mut state.application {
            queue.clear();
        }
        self.ready.notify_all();
    }
}

#[derive(Debug, Default)]
struct InboundEventQueueState {
    events: VecDeque<ChannelEvent>,
    release_all_requested: bool,
}

#[derive(Default)]
struct InboundEventQueue {
    state: Mutex<InboundEventQueueState>,
    notifier: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl InboundEventQueue {
    fn notify(&self) {
        let notifier = self.notifier.lock().ok().and_then(|value| value.clone());
        if let Some(notify) = notifier {
            notify();
        }
    }
    fn push_control(&self, event: ChannelControlEvent) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("cross-screen inbound event queue was poisoned"))?;
        if event == ChannelControlEvent::ReleaseAll {
            state.events.clear();
            state.release_all_requested = true;
            drop(state);
            self.notify();
            return Ok(());
        }
        if state
            .events
            .iter()
            .filter(|queued| matches!(queued, ChannelEvent::Control(_)))
            .count()
            >= INBOUND_CONTROL_QUEUE_CAPACITY
        {
            return Err(io::Error::other(
                "cross-screen inbound control queue reached its fixed capacity",
            ));
        }
        state.events.push_back(ChannelEvent::Control(event));
        drop(state);
        self.notify();
        Ok(())
    }

    fn push_input(&self, input: ChannelInputEvent) -> io::Result<bool> {
        validate_channel_input_event(input)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("cross-screen inbound event queue was poisoned"))?;
        let input_count = state
            .events
            .iter()
            .filter(|queued| matches!(queued, ChannelEvent::Input(_)))
            .count();
        if input_count >= INBOUND_INPUT_QUEUE_CAPACITY {
            // Reader-thread scheduling can deliver a healthy pointer stream in one burst. Keep
            // every ordinary sample; only under real fixed-capacity pressure may replaceable
            // pointer state be discarded. Keyboard, button and wheel ordering stays intact.
            if input.event.is_mouse_move()
                && let Some(ChannelEvent::Input(previous)) = state.events.back_mut()
                && previous.event.is_mouse_move()
                && previous.control_epoch == input.control_epoch
            {
                *previous = input;
                drop(state);
                self.notify();
                return Ok(true);
            }
            let Some(replaceable) = state.events.iter().position(
                |queued| matches!(queued, ChannelEvent::Input(previous) if previous.event.is_mouse_move()),
            ) else {
                return Err(io::Error::other(
                    "cross-screen inbound input queue reached its fixed capacity",
                ));
            };
            state.events.remove(replaceable);
            state.events.push_back(ChannelEvent::Input(input));
            drop(state);
            self.notify();
            return Ok(true);
        }
        state.events.push_back(ChannelEvent::Input(input));
        drop(state);
        self.notify();
        Ok(false)
    }

    fn pop(&self) -> io::Result<Option<ChannelEvent>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("cross-screen inbound event queue was poisoned"))?;
        if state.release_all_requested {
            state.release_all_requested = false;
            return Ok(Some(ChannelEvent::Control(ChannelControlEvent::ReleaseAll)));
        }
        Ok(state.events.pop_front())
    }

    fn pop_control(&self) -> io::Result<Option<ChannelControlEvent>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("cross-screen inbound event queue was poisoned"))?;
        if state.release_all_requested {
            state.release_all_requested = false;
            return Ok(Some(ChannelControlEvent::ReleaseAll));
        }
        match state.events.front() {
            Some(ChannelEvent::Control(_)) => {
                let Some(ChannelEvent::Control(event)) = state.events.pop_front() else {
                    unreachable!("front was checked as a control event");
                };
                Ok(Some(event))
            }
            Some(ChannelEvent::Input(_)) | None => Ok(None),
        }
    }

    fn pop_file_drag(&self) -> io::Result<Option<DragMessage>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("cross-screen inbound event queue was poisoned"))?;
        let Some(index) = state.events.iter().position(|event| {
            matches!(
                event,
                ChannelEvent::Control(ChannelControlEvent::FileDrag(_))
            )
        }) else {
            return Ok(None);
        };
        let Some(ChannelEvent::Control(ChannelControlEvent::FileDrag(message))) =
            state.events.remove(index)
        else {
            unreachable!("the selected event was checked as a file drag message");
        };
        Ok(Some(message))
    }

    fn pop_input(&self) -> io::Result<Option<ChannelInputEvent>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("cross-screen inbound event queue was poisoned"))?;
        if state.release_all_requested {
            return Ok(None);
        }
        match state.events.front() {
            Some(ChannelEvent::Input(_)) => {
                let Some(ChannelEvent::Input(event)) = state.events.pop_front() else {
                    unreachable!("front was checked as an input event");
                };
                Ok(Some(event))
            }
            Some(ChannelEvent::Control(_)) | None => Ok(None),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct PendingPing {
    nonce: u64,
    sent_at: Instant,
}

struct LatencyQosFlow {
    handle: usize,
    socket: usize,
    flow_id: u32,
}

impl Drop for LatencyQosFlow {
    fn drop(&mut self) {
        let handle = HANDLE(self.handle as *mut core::ffi::c_void);
        let socket = SOCKET(self.socket);
        unsafe {
            let _ = QOSRemoveSocketFromFlow(handle, Some(socket), self.flow_id, None);
            let _ = QOSCloseHandle(handle);
        }
    }
}

fn enable_latency_qos(socket: &TcpStream) -> io::Result<LatencyQosFlow> {
    let version = QOS_VERSION {
        MajorVersion: 1,
        MinorVersion: 0,
    };
    let mut handle = HANDLE::default();
    if !unsafe { QOSCreateHandle(&raw const version, &raw mut handle) }.as_bool() {
        return Err(io::Error::last_os_error());
    }
    let socket = usize::try_from(socket.as_raw_socket())
        .map_err(|_| io::Error::other("cross-screen socket handle does not fit usize"))?;
    let mut flow_id = 0_u32;
    if !unsafe {
        QOSAddSocketToFlow(
            handle,
            SOCKET(socket),
            None,
            QOSTrafficTypeControl,
            Some(QOS_NON_ADAPTIVE_FLOW),
            &raw mut flow_id,
        )
    }
    .as_bool()
    {
        let error = io::Error::last_os_error();
        unsafe {
            let _ = QOSCloseHandle(handle);
        }
        return Err(error);
    }
    Ok(LatencyQosFlow {
        handle: handle.0 as usize,
        socket,
        flow_id,
    })
}

/// Dense-heartbeat state: near-boundary bursts and sustained keep-warm.
#[derive(Debug)]
struct LinkWarmup {
    until: Option<Instant>,
    last_started: Option<Instant>,
    keep_warm_requested: bool,
    peer_allowed: bool,
}

impl LinkWarmup {
    const fn new() -> Self {
        Self {
            until: None,
            last_started: None,
            keep_warm_requested: false,
            peer_allowed: false,
        }
    }

    fn try_start(&mut self, now: Instant) -> bool {
        // Only the cooldown limits a burst. A "link quiet" precondition never held in practice
        // (periodic application traffic kept resetting it), so no burst ever fired.
        let cooled = self
            .last_started
            .is_none_or(|started| now.saturating_duration_since(started) >= WARMUP_COOLDOWN);
        if !cooled {
            return false;
        }
        self.last_started = Some(now);
        self.until = Some(now + WARMUP_DURATION);
        true
    }

    const fn keep_warm(&self, local_allows: bool) -> bool {
        self.keep_warm_requested && self.peer_allowed && local_allows
    }

    fn heartbeat_interval(&self, now: Instant, local_allows: bool) -> Duration {
        if self.until.is_some_and(|until| now < until) {
            WARMUP_HEARTBEAT_INTERVAL
        } else if self.keep_warm(local_allows) {
            KEEP_WARM_HEARTBEAT_INTERVAL
        } else {
            HEARTBEAT_INTERVAL
        }
    }
}

struct ChannelShared {
    binding: SessionBinding,
    local_fingerprint: CertificateFingerprint,
    peer_fingerprint: CertificateFingerprint,
    outbound: Arc<OutboundQueue>,
    inbound: InboundEventQueue,
    application: Mutex<[VecDeque<Vec<u8>>; 5]>,
    snapshot: Mutex<ChannelSnapshot>,
    pending_ping: Mutex<Option<PendingPing>>,
    warmup: Mutex<LinkWarmup>,
    input_cadence: InputCadenceProbe,
    control_cadence: ControlCadenceProbe,
    reader_stage: ReaderStageProbe,
    _latency_qos: Option<LatencyQosFlow>,
    stop: AtomicBool,
    local_close_sent: AtomicBool,
    peer_close_received: AtomicBool,
    shutdown_socket: TcpStream,
}

impl ChannelShared {
    fn snapshot(&self) -> ChannelSnapshot {
        self.snapshot.lock().map_or_else(
            |poisoned| poisoned.into_inner().clone(),
            |snapshot| snapshot.clone(),
        )
    }

    fn update_snapshot(&self, update: impl FnOnce(&mut ChannelSnapshot)) {
        match self.snapshot.lock() {
            Ok(mut snapshot) => update(&mut snapshot),
            Err(poisoned) => update(&mut poisoned.into_inner()),
        }
    }

    fn fail(&self, error: impl std::fmt::Display) {
        if self.stop.swap(true, Ordering::AcqRel) {
            return;
        }
        self.update_snapshot(|snapshot| {
            snapshot.state = ChannelState::Failed;
            snapshot.clean_shutdown = false;
            if snapshot.last_error.is_none() {
                snapshot.last_error = Some(error.to_string());
            }
        });
        self.outbound.close();
        let _ = self.shutdown_socket.shutdown(Shutdown::Both);
    }

    fn finish_clean(&self) {
        if self.stop.swap(true, Ordering::AcqRel) {
            return;
        }
        self.update_snapshot(|snapshot| {
            snapshot.state = ChannelState::Stopped;
            snapshot.clean_shutdown = true;
            snapshot.last_error = None;
        });
        self.outbound.close();
        let _ = self.shutdown_socket.shutdown(Shutdown::Both);
    }

    fn request_shutdown(&self) {
        self.update_snapshot(|snapshot| {
            if snapshot.state == ChannelState::Connected {
                snapshot.state = ChannelState::Closing;
            }
        });
        self.outbound.request_shutdown();
    }

    fn verify_active_frame(&self, frame: &Frame, expected_sequence: u64) -> io::Result<()> {
        if frame.sender != self.peer_fingerprint {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "cross-screen frame sender does not match the authenticated TLS peer",
            ));
        }
        if frame.session_id != self.binding.session_id
            || frame.channel_epoch != self.binding.channel_epoch
        {
            return Err(invalid_data(
                "cross-screen frame does not belong to the active channel session",
            ));
        }
        if frame.sequence != expected_sequence {
            return Err(invalid_data(format!(
                "cross-screen frame sequence mismatch: expected {expected_sequence}, got {}",
                frame.sequence
            )));
        }
        Ok(())
    }
}

pub struct CrossScreenChannel {
    shared: Arc<ChannelShared>,
    writer: Option<JoinHandle<()>>,
    reader: Option<JoinHandle<()>>,
}

impl CrossScreenChannel {
    /// Queue a bounded application message independently of input and other protocols.
    ///
    /// # Errors
    /// Rejects oversized messages, queue saturation and closing channels.
    pub fn send_application(
        &self,
        protocol: ApplicationProtocol,
        payload: Vec<u8>,
    ) -> io::Result<()> {
        self.shared.outbound.push_application(protocol, payload)?;
        if protocol == ApplicationProtocol::Control {
            self.shared.control_cadence.enqueued.observe();
        }
        Ok(())
    }

    /// Receive only the selected application protocol from this authenticated peer.
    ///
    /// # Errors
    /// Returns an error if the inbound application queue was poisoned.
    pub fn try_receive_application(
        &self,
        protocol: ApplicationProtocol,
    ) -> io::Result<Option<Vec<u8>>> {
        let message = self
            .shared
            .application
            .lock()
            .map(|mut queues| queues[protocol as usize].pop_front())
            .map_err(|_| io::Error::other("application queue poisoned"))?;
        if message.is_some() && protocol == ApplicationProtocol::Control {
            self.shared.control_cadence.dequeued.observe();
        }
        Ok(message)
    }
    /// Connects one independent, pinned mutual-TLS cross-screen channel.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe endpoint, missing or revoked trust, TLS failure, protocol
    /// handshake failure, or worker startup failure.
    pub fn connect(
        store: &DeviceStore,
        peer_fingerprint: CertificateFingerprint,
        address: SocketAddr,
        timeout: Duration,
    ) -> io::Result<Self> {
        validate_connect_address(address)?;
        validate_channel_timeout(timeout)?;
        let socket = TcpStream::connect_timeout(&address, timeout)?;
        Self::connect_socket(store, peer_fingerprint, socket, timeout)
    }

    pub(crate) fn connect_socket(
        store: &DeviceStore,
        peer_fingerprint: CertificateFingerprint,
        socket: TcpStream,
        timeout: Duration,
    ) -> io::Result<Self> {
        validate_connect_address(socket.peer_addr()?)?;
        validate_channel_timeout(timeout)?;
        let identity = store.load_identity()?;
        let local_fingerprint = identity.fingerprint();
        let peer = store.load_peer(peer_fingerprint)?;
        let pinned_certificate = peer.certificate_der().to_vec();
        let tls = PinnedTlsClient::new(
            &identity,
            pinned_certificate.clone(),
            peer_fingerprint,
            timeout,
        )?;
        let mut stream = tls.connect_socket(socket)?;
        let binding = SessionBinding::generate()?;
        let hello = Frame {
            kind: FrameKind::Hello,
            sender: local_fingerprint,
            session_id: binding.session_id,
            channel_epoch: binding.channel_epoch,
            sequence: 1,
            payload: Vec::new(),
        };
        write_handshake_frame(&mut stream, &hello)?;
        let acknowledgement = read_handshake_frame(&mut stream)?;
        verify_handshake_frame(
            &acknowledgement,
            FrameKind::HelloAcknowledgement,
            peer_fingerprint,
            binding,
        )?;
        recheck_trust(store, peer_fingerprint, &pinned_certificate)?;
        let local_address = stream.sock.local_addr()?;
        let peer_address = stream.sock.peer_addr()?;
        let (connection, socket) = stream.into_parts();
        start_channel(
            Connection::Client(connection),
            socket,
            ChannelMetadata {
                role: ChannelRole::Connector,
                local_fingerprint,
                peer_fingerprint,
                local_address,
                peer_address,
                binding,
            },
        )
    }

    /// Accepts one independent, pinned mutual-TLS cross-screen channel before the timeout.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe listener, missing or revoked trust, timeout, TLS failure,
    /// protocol handshake failure, or worker startup failure.
    pub fn listen_once(
        store: &DeviceStore,
        peer_fingerprint: CertificateFingerprint,
        address: SocketAddr,
        timeout: Duration,
    ) -> io::Result<Self> {
        validate_listener_address(address)?;
        validate_channel_timeout(timeout)?;
        let listener = TcpListener::bind(address)?;
        accept_from_listener(store, peer_fingerprint, &listener, timeout)
    }

    /// Replaces any unsent mouse position with the newest normalized primary-screen position.
    ///
    /// # Errors
    ///
    /// Returns an error after shutdown begins or if the bounded queue is unavailable.
    pub fn send_mouse_position(&self, position: NormalizedMousePosition) -> io::Result<()> {
        let coalesced = self.shared.outbound.push_mouse_move(position)?;
        if coalesced {
            self.shared
                .update_snapshot(|snapshot| snapshot.coalesced_mouse_moves += 1);
        }
        Ok(())
    }

    /// Reliably publishes this endpoint's physical primary-screen dimensions.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid dimensions, shutdown, or a full bounded reliable queue.
    pub fn send_screen_info(&self, screen: PhysicalScreenSize) -> io::Result<()> {
        PhysicalScreenSize::new(screen.width, screen.height)?;
        self.shared
            .outbound
            .push_reliable(ReliableMessage::ScreenInfo(screen))
    }

    /// Reliably publishes this endpoint's operating-system reported physical display size.
    ///
    /// `None` explicitly tells the peer that the driver did not provide usable millimeter data.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid dimensions, shutdown, or a full bounded reliable queue.
    pub fn send_screen_millimeters(&self, size: Option<ScreenMillimeterSize>) -> io::Result<()> {
        if let Some(size) = size {
            ScreenMillimeterSize::new(size.width, size.height)?;
        }
        self.shared
            .outbound
            .push_reliable(ReliableMessage::ScreenMillimeters(size))
    }

    /// Reliably publishes the revisioned pair layout used by both endpoints.
    ///
    /// # Errors
    ///
    /// Returns an error after shutdown begins or when the bounded queue is full.
    pub fn send_layout_info(&self, layout: LayoutConfiguration) -> io::Result<()> {
        if layout.revision == u64::MAX || layout.corner_guard_pixels > 96 {
            return Err(invalid_data(
                "cross-screen layout configuration is outside its bounds",
            ));
        }
        self.shared
            .outbound
            .push_reliable(ReliableMessage::LayoutInfo(layout))
    }

    /// Reliably publishes one bounded, authenticated boundary-workspace protocol message.
    ///
    /// The payload is opaque to the transport and validated by the boundary sync layer.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty or oversized message, shutdown, or a full queue.
    pub fn send_boundary_workspace(&self, payload: Vec<u8>) -> io::Result<()> {
        if payload.is_empty() || payload.len() > MAX_FRAME_PAYLOAD {
            return Err(invalid_data(
                "cross-screen boundary workspace payload is outside its bounds",
            ));
        }
        self.shared
            .outbound
            .push_reliable(ReliableMessage::BoundaryWorkspace(payload))
    }

    /// Reliably requests transfer of control to the peer.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero control epoch, shutdown, or a full bounded reliable queue.
    pub fn send_enter_request(&self, request: ControlTransitionRequest) -> io::Result<()> {
        validate_transition_request(&request)?;
        self.shared
            .outbound
            .push_reliable(ReliableMessage::EnterRequest(request))
    }

    pub(crate) fn send_file_drag(&self, message: DragMessage) -> io::Result<()> {
        message.encode()?;
        self.shared
            .outbound
            .push_reliable(ReliableMessage::FileDrag(message))
    }

    /// Reliably accepts or rejects one exact enter request.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero control epoch, shutdown, or a full bounded reliable queue.
    pub fn send_enter_acknowledgement(
        &self,
        acknowledgement: ControlTransitionAck,
    ) -> io::Result<()> {
        validate_transition_acknowledgement(acknowledgement)?;
        self.shared
            .outbound
            .push_reliable(ReliableMessage::EnterAcknowledgement(acknowledgement))
    }

    /// Reliably requests return of the current control epoch to its owning endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero control epoch, shutdown, or a full bounded reliable queue.
    pub fn send_return_request(&self, request: ControlTransitionRequest) -> io::Result<()> {
        validate_transition_request(&request)?;
        self.shared
            .outbound
            .push_reliable(ReliableMessage::ReturnRequest(request))
    }

    /// Reliably accepts or rejects one exact return request.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero control epoch, shutdown, or a full bounded reliable queue.
    pub fn send_return_acknowledgement(
        &self,
        acknowledgement: ControlTransitionAck,
    ) -> io::Result<()> {
        validate_transition_acknowledgement(acknowledgement)?;
        self.shared
            .outbound
            .push_reliable(ReliableMessage::ReturnAcknowledgement(acknowledgement))
    }

    /// Reliably queues one control-epoch-bound keyboard, button, wheel, or latest mouse event.
    ///
    /// Consecutive mouse moves from the same control epoch are coalesced without crossing a
    /// keyboard, button, or wheel ordering barrier.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid event, shutdown, or a full bounded reliable queue.
    pub fn send_input_event(&self, input: ChannelInputEvent) -> io::Result<()> {
        let is_mouse_move = matches!(input.event, RemoteInputEvent::MouseMove(_));
        let coalesced = self.shared.outbound.push_input(input)?;
        if is_mouse_move {
            self.shared.input_cadence.enqueued.observe();
        }
        if coalesced {
            self.shared
                .update_snapshot(|snapshot| snapshot.coalesced_input_mouse_moves += 1);
        }
        Ok(())
    }

    /// Removes the oldest decoded control event without blocking.
    ///
    /// # Errors
    ///
    /// Returns an error only if the bounded inbound queue was poisoned.
    pub fn try_receive_control_event(&self) -> io::Result<Option<ChannelControlEvent>> {
        self.shared.inbound.pop_control()
    }

    pub(crate) fn try_receive_file_drag(&self) -> io::Result<Option<DragMessage>> {
        self.shared.inbound.pop_file_drag()
    }

    /// Removes the oldest authenticated input event without blocking.
    ///
    /// The caller must still require the exact currently active control epoch before injection.
    ///
    /// # Errors
    ///
    /// Returns an error only if the bounded inbound queue was poisoned.
    pub fn try_receive_input_event(&self) -> io::Result<Option<ChannelInputEvent>> {
        self.shared.inbound.pop_input()
    }

    /// Removes the next authenticated control or input event in original TLS/TCP order.
    ///
    /// Release-all is the deliberate high-priority exception: it clears queued input and is
    /// delivered first so no stale input can follow a forced release.
    ///
    /// # Errors
    ///
    /// Returns an error only if the bounded inbound queue was poisoned.
    pub fn try_receive_event(&self) -> io::Result<Option<ChannelEvent>> {
        self.shared.inbound.pop()
    }

    pub(super) fn set_event_notifier(&self, notifier: Arc<dyn Fn() + Send + Sync>) {
        *self
            .shared
            .inbound
            .notifier
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(notifier);
        self.shared.inbound.notify();
    }

    /// Queues an idempotent release-all control frame ahead of mouse movement.
    pub fn request_release_all(&self) {
        self.shared.outbound.request_release_all();
    }

    /// Starts a short dense-heartbeat window when this link has carried no real traffic for a
    /// while (see `WARMUP_HEARTBEAT_INTERVAL`). Returns whether a new window started.
    #[must_use]
    pub fn request_warmup(&self) -> bool {
        if !super::link_warmth::enabled() {
            return false;
        }
        let started = self
            .shared
            .warmup
            .lock()
            .is_ok_and(|mut warmup| warmup.try_start(Instant::now()));
        if started {
            self.shared.outbound.request_heartbeat();
        }
        started
    }

    /// Requests sustained keep-warm for this link. It takes effect only while this end allows it
    /// and the peer advertised that it allows it too; returns whether it is now in effect.
    #[must_use]
    pub fn set_keep_warm(&self, requested: bool) -> bool {
        let local_allows = super::link_warmth::local_allows();
        let Ok(mut warmup) = self.shared.warmup.lock() else {
            return false;
        };
        let previous = std::mem::replace(&mut warmup.keep_warm_requested, requested);
        let effective = warmup.keep_warm(local_allows);
        drop(warmup);
        if requested && !previous {
            self.shared.outbound.request_heartbeat();
        }
        effective
    }

    #[must_use]
    pub fn snapshot(&self) -> ChannelSnapshot {
        self.shared.snapshot()
    }

    pub(super) fn take_input_cadence(&self) -> InputCadenceSnapshot {
        self.shared.input_cadence.take()
    }

    pub(crate) fn reset_control_cadence(&self) {
        self.shared.control_cadence.reset();
        self.shared.reader_stage.reset();
    }

    pub(crate) fn take_control_cadence(&self) -> ControlCadenceSnapshot {
        self.shared.control_cadence.take()
    }

    pub(crate) fn take_reader_stage(&self) -> ReaderStageSnapshot {
        self.shared.reader_stage.take()
    }

    pub(crate) fn tcp_path_snapshot(&self) -> io::Result<TcpPathSnapshot> {
        tcp_path_snapshot(&self.shared.shutdown_socket)
    }

    /// Sends release-all and close frames, waits for the authenticated peer close response, and
    /// joins both I/O workers.
    pub fn stop(&mut self) {
        if self.writer.is_none() && self.reader.is_none() {
            return;
        }
        if !self.shared.stop.load(Ordering::Acquire) {
            self.shared.request_shutdown();
        }
        if let Some(writer) = self.writer.take()
            && writer.join().is_err()
        {
            self.shared.fail("cross-screen writer thread panicked");
        }
        if let Some(reader) = self.reader.take()
            && reader.join().is_err()
        {
            self.shared.fail("cross-screen reader thread panicked");
        }
    }

    pub(crate) fn request_close(&self) {
        self.shared.request_shutdown();
    }

    pub(crate) fn workers_finished(&self) -> bool {
        self.writer.as_ref().is_none_or(JoinHandle::is_finished)
            && self.reader.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

impl Drop for CrossScreenChannel {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A hub authenticates a socket before reading the bounded channel hello. Trust
/// alone is insufficient: only enabled workspace members may own a channel.
pub(crate) fn accept_workspace_socket(
    store: &DeviceStore,
    socket: TcpStream,
    allowed: &[CertificateFingerprint],
    timeout: Duration,
    stop: &AtomicBool,
) -> io::Result<(CertificateFingerprint, CrossScreenChannel)> {
    validate_peer_ip(socket.peer_addr()?.ip())?;
    let identity = store.load_identity()?;
    let local = identity.fingerprint();
    let tls = TrustedTlsServer::new(identity, store.clone(), timeout)?;
    let authenticated = tls.accept_cancellable(socket, stop)?;
    let peer = authenticated.peer_fingerprint;
    if !allowed.contains(&peer) || peer.bytes() >= local.bytes() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "peer is not an enabled workspace dialer",
        ));
    }
    let certificate = store.load_peer(peer)?.certificate_der().to_vec();
    finish_accepted_channel(store, local, peer, &certificate, authenticated.stream)
        .map(|channel| (peer, channel))
}

pub(super) fn accept_from_listener(
    store: &DeviceStore,
    peer_fingerprint: CertificateFingerprint,
    listener: &TcpListener,
    timeout: Duration,
) -> io::Result<CrossScreenChannel> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| invalid_data("cross-screen accept timeout overflow"))?;
    let (socket, peer_address) = loop {
        match listener.accept() {
            Ok(pair) => break pair,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "cross-screen listener timed out",
                    ));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error),
        }
    };
    validate_peer_ip(peer_address.ip())?;
    let identity = store.load_identity()?;
    let local_fingerprint = identity.fingerprint();
    let peer = store.load_peer(peer_fingerprint)?;
    let pinned_certificate = peer.certificate_der().to_vec();
    let tls = PinnedTlsServer::new(
        &identity,
        pinned_certificate.clone(),
        peer_fingerprint,
        timeout,
    )?;
    let stream = tls.accept(socket)?;
    finish_accepted_channel(
        store,
        local_fingerprint,
        peer_fingerprint,
        &pinned_certificate,
        stream,
    )
}

fn finish_accepted_channel(
    store: &DeviceStore,
    local_fingerprint: CertificateFingerprint,
    peer_fingerprint: CertificateFingerprint,
    pinned_certificate: &[u8],
    mut stream: StreamOwned<rustls::ServerConnection, TcpStream>,
) -> io::Result<CrossScreenChannel> {
    let hello = read_handshake_frame(&mut stream)?;
    if hello.kind != FrameKind::Hello
        || hello.sender != peer_fingerprint
        || hello.sequence != 1
        || !hello.payload.is_empty()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "cross-screen hello does not match the authenticated TLS peer",
        ));
    }
    let binding = SessionBinding {
        session_id: hello.session_id,
        channel_epoch: hello.channel_epoch,
    };
    let acknowledgement = Frame {
        kind: FrameKind::HelloAcknowledgement,
        sender: local_fingerprint,
        session_id: binding.session_id,
        channel_epoch: binding.channel_epoch,
        sequence: 1,
        payload: Vec::new(),
    };
    write_handshake_frame(&mut stream, &acknowledgement)?;
    recheck_trust(store, peer_fingerprint, pinned_certificate)?;
    let local_address = stream.sock.local_addr()?;
    let peer_address = stream.sock.peer_addr()?;
    let (connection, socket) = stream.into_parts();
    start_channel(
        Connection::Server(connection),
        socket,
        ChannelMetadata {
            role: ChannelRole::Listener,
            local_fingerprint,
            peer_fingerprint,
            local_address,
            peer_address,
            binding,
        },
    )
}

#[derive(Clone, Copy, Debug)]
struct ChannelMetadata {
    role: ChannelRole,
    local_fingerprint: CertificateFingerprint,
    peer_fingerprint: CertificateFingerprint,
    local_address: SocketAddr,
    peer_address: SocketAddr,
    binding: SessionBinding,
}

fn prepare_channel_socket(
    socket: &TcpStream,
) -> io::Result<(Option<LatencyQosFlow>, Option<String>)> {
    socket.set_nonblocking(false)?;
    socket.set_nodelay(true)?;
    socket.set_read_timeout(Some(READER_POLL_INTERVAL))?;
    socket.set_write_timeout(Some(WRITE_TIMEOUT))?;
    if !super::link_warmth::qos_marking() {
        return Ok((None, Some("disabled_by_flag".to_owned())));
    }
    Ok(match enable_latency_qos(socket) {
        Ok(flow) => (Some(flow), None),
        Err(error) => (None, Some(error.to_string())),
    })
}

fn start_channel(
    connection: Connection,
    socket: TcpStream,
    metadata: ChannelMetadata,
) -> io::Result<CrossScreenChannel> {
    let ChannelMetadata {
        role,
        local_fingerprint,
        peer_fingerprint,
        local_address,
        peer_address,
        binding,
    } = metadata;
    let (latency_qos, latency_qos_error) = prepare_channel_socket(&socket)?;
    let latency_qos_enabled = latency_qos.is_some();
    let reader_socket = socket.try_clone()?;
    let shutdown_socket = socket.try_clone()?;
    let connection = Arc::new(Mutex::new(connection));
    let outbound = Arc::new(OutboundQueue::default());
    let snapshot = ChannelSnapshot {
        role,
        state: ChannelState::Connected,
        local_fingerprint,
        peer_fingerprint,
        local_address,
        peer_address,
        session_id: encode_hex(&binding.session_id),
        channel_epoch: binding.channel_epoch,
        sent_frames: 1,
        received_frames: 1,
        sent_mouse_moves: 0,
        received_mouse_moves: 0,
        coalesced_mouse_moves: 0,
        sent_heartbeats: 0,
        received_heartbeats: 0,
        sent_release_all: 0,
        received_release_all: 0,
        sent_control_messages: 0,
        received_control_messages: 0,
        sent_input_events: 0,
        received_input_events: 0,
        coalesced_input_mouse_moves: 0,
        last_sent_sequence: 1,
        last_received_sequence: 1,
        round_trip_micros: None,
        latency_qos_enabled,
        latency_qos_error,
        clean_shutdown: false,
        last_error: None,
    };
    let shared = Arc::new(ChannelShared {
        binding,
        local_fingerprint,
        peer_fingerprint,
        outbound,
        inbound: InboundEventQueue::default(),
        application: Mutex::default(),
        snapshot: Mutex::new(snapshot),
        pending_ping: Mutex::new(None),
        warmup: Mutex::new(LinkWarmup::new()),
        input_cadence: InputCadenceProbe::default(),
        control_cadence: ControlCadenceProbe::default(),
        reader_stage: ReaderStageProbe::default(),
        _latency_qos: latency_qos,
        stop: AtomicBool::new(false),
        local_close_sent: AtomicBool::new(false),
        peer_close_received: AtomicBool::new(false),
        shutdown_socket,
    });

    let writer_shared = Arc::clone(&shared);
    let writer_connection = Arc::clone(&connection);
    let writer = std::thread::Builder::new()
        .name("pierdesk-cross-screen-writer".to_owned())
        .spawn(move || {
            #[cfg(not(test))]
            crate::traffic_control::promote_interactive_thread();
            if let Err(error) = writer_loop(&writer_shared, &writer_connection, socket) {
                writer_shared.fail(format!("cross-screen writer failed: {error}"));
            }
        })?;

    let reader_shared = Arc::clone(&shared);
    let reader_connection = Arc::clone(&connection);
    let reader = std::thread::Builder::new()
        .name("pierdesk-cross-screen-reader".to_owned())
        .spawn(move || {
            #[cfg(not(test))]
            crate::traffic_control::promote_interactive_thread();
            if let Err(error) = reader_loop(&reader_shared, &reader_connection, reader_socket) {
                reader_shared.fail(format!("cross-screen reader failed: {error}"));
            }
        });
    let reader = match reader {
        Ok(reader) => reader,
        Err(error) => {
            shared.fail(format!("cross-screen reader could not start: {error}"));
            let _ = writer.join();
            return Err(error);
        }
    };
    Ok(CrossScreenChannel {
        shared,
        writer: Some(writer),
        reader: Some(reader),
    })
}

fn write_handshake_frame<C>(stream: &mut StreamOwned<C, TcpStream>, frame: &Frame) -> io::Result<()>
where
    StreamOwned<C, TcpStream>: Write,
{
    stream.write_all(&frame.encode()?)?;
    stream.flush()
}

fn read_handshake_frame<C>(stream: &mut StreamOwned<C, TcpStream>) -> io::Result<Frame>
where
    StreamOwned<C, TcpStream>: Read,
{
    let mut header = [0_u8; FRAME_HEADER_LENGTH];
    stream.read_exact(&mut header)?;
    let payload_length = usize::from(read_u16(&header, 12)?);
    if payload_length > MAX_FRAME_PAYLOAD {
        return Err(invalid_data(
            "cross-screen handshake payload exceeds its limit",
        ));
    }
    let mut encoded = Vec::with_capacity(FRAME_HEADER_LENGTH + payload_length);
    encoded.extend_from_slice(&header);
    encoded.resize(FRAME_HEADER_LENGTH + payload_length, 0);
    stream.read_exact(&mut encoded[FRAME_HEADER_LENGTH..])?;
    Frame::decode(&encoded)
}

fn verify_handshake_frame(
    frame: &Frame,
    expected_kind: FrameKind,
    expected_peer: CertificateFingerprint,
    binding: SessionBinding,
) -> io::Result<()> {
    if frame.kind != expected_kind
        || frame.sender != expected_peer
        || frame.session_id != binding.session_id
        || frame.channel_epoch != binding.channel_epoch
        || frame.sequence != 1
        || !frame.payload.is_empty()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "cross-screen application handshake did not match the pinned TLS session",
        ));
    }
    Ok(())
}

fn recheck_trust(
    store: &DeviceStore,
    peer_fingerprint: CertificateFingerprint,
    pinned_certificate: &[u8],
) -> io::Result<()> {
    let current = store.load_peer(peer_fingerprint).map_err(|error| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("cross-screen peer trust was revoked: {error}"),
        )
    })?;
    if current.certificate_der() != pinned_certificate {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "cross-screen peer certificate changed during connection setup",
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_lines)] // Keep the writer's frame ordering visible in one loop.
fn writer_loop(
    shared: &ChannelShared,
    connection: &Mutex<Connection>,
    mut socket: TcpStream,
) -> io::Result<()> {
    let mut next_sequence = 2_u64;
    let mut last_heartbeat = Instant::now();
    let mut shutdown_deadline = None;
    loop {
        if shared.stop.load(Ordering::Acquire) {
            return Ok(());
        }
        if shared.local_close_sent.load(Ordering::Acquire) {
            if shared.peer_close_received.load(Ordering::Acquire) {
                // Both authenticated application close frames are already exchanged. A racing
                // peer TCP shutdown can make close_notify unwritable, so it is best-effort only.
                let _ = send_tls_close_notify(connection, &mut socket);
                shared.finish_clean();
                return Ok(());
            }
            let deadline = shutdown_deadline.expect("close frame sets its shutdown deadline");
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "peer did not acknowledge cross-screen shutdown",
                ));
            }
            let _ = shared
                .outbound
                .pop_until((Instant::now() + Duration::from_millis(100)).min(deadline))?;
            continue;
        }

        let next_heartbeat = next_heartbeat_deadline(shared, last_heartbeat);
        let item = shared.outbound.pop_until(next_heartbeat)?;
        match item {
            Some(QueueItem::MouseMove(position)) => {
                send_active_frame(
                    shared,
                    connection,
                    &mut socket,
                    FrameKind::MouseMove,
                    mouse_payload(position),
                    &mut next_sequence,
                )?;
            }
            Some(QueueItem::Reliable(message)) => {
                let (kind, payload) = reliable_frame(message);
                send_active_frame(
                    shared,
                    connection,
                    &mut socket,
                    kind,
                    payload,
                    &mut next_sequence,
                )?;
            }
            Some(QueueItem::Application(message)) => {
                let mut payload = Vec::with_capacity(message.payload.len() + 1);
                payload.push(message.protocol as u8);
                payload.extend_from_slice(&message.payload);
                send_active_frame(
                    shared,
                    connection,
                    &mut socket,
                    FrameKind::Application,
                    payload,
                    &mut next_sequence,
                )?;
            }
            Some(QueueItem::ReleaseAll) => {
                send_active_frame(
                    shared,
                    connection,
                    &mut socket,
                    FrameKind::ReleaseAll,
                    Vec::new(),
                    &mut next_sequence,
                )?;
            }
            Some(QueueItem::Shutdown) => {
                send_active_frame(
                    shared,
                    connection,
                    &mut socket,
                    FrameKind::ReleaseAll,
                    Vec::new(),
                    &mut next_sequence,
                )?;
                send_active_frame(
                    shared,
                    connection,
                    &mut socket,
                    FrameKind::Close,
                    Vec::new(),
                    &mut next_sequence,
                )?;
                shared.local_close_sent.store(true, Ordering::Release);
                shutdown_deadline = Some(Instant::now() + GRACEFUL_SHUTDOWN_TIMEOUT);
                shared.update_snapshot(|snapshot| snapshot.state = ChannelState::Closing);
                shared.outbound.wake();
            }
            None => {}
        }

        if Instant::now() >= next_heartbeat && !shared.local_close_sent.load(Ordering::Acquire) {
            send_ping_if_idle(shared, connection, &mut socket, &mut next_sequence)?;
            last_heartbeat = Instant::now();
        }
    }
}

fn next_heartbeat_deadline(shared: &ChannelShared, last_heartbeat: Instant) -> Instant {
    let interval = shared.warmup.lock().map_or(HEARTBEAT_INTERVAL, |warmup| {
        warmup.heartbeat_interval(Instant::now(), super::link_warmth::local_allows())
    });
    last_heartbeat + interval
}

fn send_ping_if_idle(
    shared: &ChannelShared,
    connection: &Mutex<Connection>,
    socket: &mut TcpStream,
    next_sequence: &mut u64,
) -> io::Result<()> {
    let nonce = if super::link_warmth::local_allows() {
        *next_sequence | PING_WARM_ALLOWED
    } else {
        *next_sequence
    };
    {
        let mut pending = shared
            .pending_ping
            .lock()
            .map_err(|_| io::Error::other("cross-screen ping state was poisoned"))?;
        if pending.is_some() {
            return Ok(());
        }
        *pending = Some(PendingPing {
            nonce,
            sent_at: Instant::now(),
        });
    }
    if let Err(error) = send_active_frame(
        shared,
        connection,
        socket,
        FrameKind::Ping,
        nonce.to_le_bytes().to_vec(),
        next_sequence,
    ) {
        if let Ok(mut pending) = shared.pending_ping.lock() {
            *pending = None;
        }
        return Err(error);
    }
    Ok(())
}

fn send_active_frame(
    shared: &ChannelShared,
    connection: &Mutex<Connection>,
    socket: &mut TcpStream,
    kind: FrameKind,
    payload: Vec<u8>,
    next_sequence: &mut u64,
) -> io::Result<()> {
    let sequence = *next_sequence;
    let following = sequence
        .checked_add(1)
        .ok_or_else(|| invalid_data("cross-screen send sequence overflow"))?;
    let frame = Frame {
        kind,
        sender: shared.local_fingerprint,
        session_id: shared.binding.session_id,
        channel_epoch: shared.binding.channel_epoch,
        sequence,
        payload,
    };
    let encoded = frame.encode()?;
    let encrypted = {
        let mut connection = connection
            .lock()
            .map_err(|_| io::Error::other("cross-screen TLS state was poisoned"))?;
        connection.writer().write_all(&encoded)?;
        drain_tls(&mut connection)?
    };
    // Socket backpressure must not hold the TLS mutex and block inbound input/acks.
    // The channel writer is the sole socket writer, so record ordering is preserved.
    socket.write_all(&encrypted)?;
    if kind == FrameKind::InputMouseMove {
        shared.input_cadence.sent.observe();
    }
    if kind == FrameKind::Application
        && frame.payload.first() == Some(&(ApplicationProtocol::Control as u8))
    {
        shared.control_cadence.sent.observe();
    }
    *next_sequence = following;
    shared.update_snapshot(|snapshot| {
        snapshot.sent_frames += 1;
        snapshot.last_sent_sequence = sequence;
        match kind {
            FrameKind::MouseMove => snapshot.sent_mouse_moves += 1,
            FrameKind::Ping => snapshot.sent_heartbeats += 1,
            FrameKind::ReleaseAll => snapshot.sent_release_all += 1,
            FrameKind::Application
            | FrameKind::FileDrag
            | FrameKind::ScreenInfo
            | FrameKind::ScreenMillimeters
            | FrameKind::LayoutInfo
            | FrameKind::BoundaryWorkspace
            | FrameKind::EnterRequest
            | FrameKind::EnterAcknowledgement
            | FrameKind::ReturnRequest
            | FrameKind::ReturnAcknowledgement => snapshot.sent_control_messages += 1,
            FrameKind::InputKey
            | FrameKind::InputMouseMove
            | FrameKind::InputMouseButton
            | FrameKind::InputWheel => snapshot.sent_input_events += 1,
            FrameKind::Hello
            | FrameKind::HelloAcknowledgement
            | FrameKind::Pong
            | FrameKind::Close => {}
        }
    });
    Ok(())
}

fn drain_tls(connection: &mut Connection) -> io::Result<Vec<u8>> {
    let mut encrypted = Vec::new();
    while connection.wants_write() {
        let written = connection.write_tls(&mut encrypted)?;
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "cross-screen TLS socket wrote zero bytes",
            ));
        }
    }
    Ok(encrypted)
}

fn send_tls_close_notify(connection: &Mutex<Connection>, socket: &mut TcpStream) -> io::Result<()> {
    let encrypted = {
        let mut connection = connection
            .lock()
            .map_err(|_| io::Error::other("cross-screen TLS state was poisoned"))?;
        connection.send_close_notify();
        drain_tls(&mut connection)?
    };
    socket.write_all(&encrypted)
}

fn reader_loop(
    shared: &ChannelShared,
    connection: &Mutex<Connection>,
    mut socket: TcpStream,
) -> io::Result<()> {
    let mut encrypted = [0_u8; 4_096];
    let mut decoder = FrameDecoder::default();
    let mut expected_sequence = 2_u64;
    let mut last_inbound = Instant::now();
    let mut limiter = FrameRateLimiter::new();
    loop {
        if shared.stop.load(Ordering::Acquire) {
            return Ok(());
        }
        shared.reader_stage.begin_iteration();
        let read_started = Instant::now();
        match socket.read(&mut encrypted) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "cross-screen peer closed without an authenticated close frame",
                ));
            }
            Ok(length) => {
                shared
                    .reader_stage
                    .socket_read(read_started.elapsed(), length, false);
                last_inbound = Instant::now();
                let tls_started = Instant::now();
                let (plaintext, tls_closed) = decrypt_tls(connection, &encrypted[..length])?;
                shared.reader_stage.tls_finished(tls_started.elapsed());
                let dispatch_started = Instant::now();
                for frame in decoder.push(&plaintext)? {
                    limiter.observe()?;
                    let closed = handle_received_frame(shared, &frame, &mut expected_sequence)?;
                    if closed {
                        return Ok(());
                    }
                }
                shared
                    .reader_stage
                    .dispatch_finished(dispatch_started.elapsed());
                shared.reader_stage.end_iteration();
                if tls_closed {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "cross-screen TLS closed before its authenticated close frame",
                    ));
                }
            }
            Err(error) if is_timeout(&error) => {
                shared
                    .reader_stage
                    .socket_read(read_started.elapsed(), 0, true);
                shared.reader_stage.end_iteration();
                if last_inbound.elapsed() >= HEARTBEAT_TIMEOUT {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "cross-screen heartbeat timed out",
                    ));
                }
            }
            Err(error) => return Err(error),
        }
    }
}

fn decrypt_tls(connection: &Mutex<Connection>, encrypted: &[u8]) -> io::Result<(Vec<u8>, bool)> {
    let mut connection = connection
        .lock()
        .map_err(|_| io::Error::other("cross-screen TLS state was poisoned"))?;
    let mut cursor = Cursor::new(encrypted);
    while usize::try_from(cursor.position()).unwrap_or(usize::MAX) < encrypted.len() {
        let read = connection.read_tls(&mut cursor)?;
        if read == 0 {
            break;
        }
    }
    connection
        .process_new_packets()
        .map_err(|error| invalid_data(format!("invalid cross-screen TLS record: {error}")))?;
    let mut plaintext = Vec::new();
    let mut buffer = [0_u8; 4_096];
    let mut tls_closed = false;
    loop {
        match connection.reader().read(&mut buffer) {
            Ok(0) => {
                tls_closed = true;
                break;
            }
            Ok(length) => {
                if plaintext.len().saturating_add(length) > MAX_DECODE_BUFFER {
                    return Err(invalid_data(
                        "cross-screen TLS plaintext exceeded its fixed limit",
                    ));
                }
                plaintext.extend_from_slice(&buffer[..length]);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) => return Err(error),
        }
    }
    Ok((plaintext, tls_closed))
}

#[allow(clippy::too_many_lines)]
fn handle_received_frame(
    shared: &ChannelShared,
    frame: &Frame,
    expected_sequence: &mut u64,
) -> io::Result<bool> {
    if shared.peer_close_received.load(Ordering::Acquire) {
        return Err(invalid_data(
            "cross-screen peer sent a frame after its close frame",
        ));
    }
    accept_received_sequence(shared, frame, expected_sequence)?;
    match frame.kind {
        FrameKind::Application => {
            let protocol = ApplicationProtocol::parse(frame.payload[0])?;
            let mut queues = shared
                .application
                .lock()
                .map_err(|_| io::Error::other("application queue poisoned"))?;
            let queue = &mut queues[protocol as usize];
            if queue.len() >= application_queue_capacity(protocol) {
                return Err(invalid_data("inbound workspace protocol queue is full"));
            }
            queue.push_back(frame.payload[1..].to_vec());
            drop(queues);
            if protocol == ApplicationProtocol::Control {
                shared.control_cadence.received.observe();
            }
            shared.inbound.notify();
            shared.update_snapshot(|snapshot| snapshot.received_control_messages += 1);
        }
        FrameKind::MouseMove => {
            let _ = decode_mouse_payload(&frame.payload)?;
            shared.update_snapshot(|snapshot| snapshot.received_mouse_moves += 1);
        }
        FrameKind::Ping => {
            let nonce = read_u64(&frame.payload, 0)?;
            if let Ok(mut warmup) = shared.warmup.lock() {
                warmup.peer_allowed = nonce & PING_WARM_ALLOWED != 0;
            }
            if let Err(error) = shared.outbound.push_reliable(ReliableMessage::Pong(nonce))
                && (error.kind() != io::ErrorKind::BrokenPipe
                    || shared.snapshot().state == ChannelState::Connected)
            {
                return Err(error);
            }
            shared.update_snapshot(|snapshot| snapshot.received_heartbeats += 1);
        }
        FrameKind::Pong => {
            let nonce = read_u64(&frame.payload, 0)?;
            let pending = shared
                .pending_ping
                .lock()
                .map_err(|_| io::Error::other("cross-screen ping state was poisoned"))?
                .take();
            let Some(pending) = pending.filter(|pending| pending.nonce == nonce) else {
                return Err(invalid_data(
                    "cross-screen pong does not match the outstanding ping",
                ));
            };
            let micros = u64::try_from(pending.sent_at.elapsed().as_micros()).unwrap_or(u64::MAX);
            #[cfg(not(test))]
            crate::traffic_control::observe_cross_screen_rtt(micros);
            shared.update_snapshot(|snapshot| snapshot.round_trip_micros = Some(micros));
        }
        FrameKind::ReleaseAll => {
            shared
                .inbound
                .push_control(ChannelControlEvent::ReleaseAll)?;
            shared.update_snapshot(|snapshot| snapshot.received_release_all += 1);
        }
        FrameKind::FileDrag => {
            shared
                .inbound
                .push_control(ChannelControlEvent::FileDrag(DragMessage::decode(
                    &frame.payload,
                )?))?;
        }
        FrameKind::ScreenInfo => {
            let screen = decode_screen_info_payload(&frame.payload)?;
            shared
                .inbound
                .push_control(ChannelControlEvent::ScreenInfo(screen))?;
            shared.update_snapshot(|snapshot| snapshot.received_control_messages += 1);
        }
        FrameKind::ScreenMillimeters => {
            let size = decode_screen_millimeters_payload(&frame.payload)?;
            shared
                .inbound
                .push_control(ChannelControlEvent::ScreenMillimeters(size))?;
            shared.update_snapshot(|snapshot| snapshot.received_control_messages += 1);
        }
        FrameKind::LayoutInfo => {
            let layout = decode_layout_info_payload(&frame.payload)?;
            shared
                .inbound
                .push_control(ChannelControlEvent::LayoutInfo(layout))?;
            shared.update_snapshot(|snapshot| snapshot.received_control_messages += 1);
        }
        FrameKind::BoundaryWorkspace => {
            shared
                .inbound
                .push_control(ChannelControlEvent::BoundaryWorkspace(
                    frame.payload.clone(),
                ))?;
            shared.update_snapshot(|snapshot| snapshot.received_control_messages += 1);
        }
        FrameKind::EnterRequest => {
            let request = decode_transition_request_payload(&frame.payload)?;
            shared
                .inbound
                .push_control(ChannelControlEvent::EnterRequest(request))?;
            shared.update_snapshot(|snapshot| snapshot.received_control_messages += 1);
        }
        FrameKind::EnterAcknowledgement => {
            let acknowledgement = decode_transition_acknowledgement_payload(&frame.payload)?;
            shared
                .inbound
                .push_control(ChannelControlEvent::EnterAcknowledgement(acknowledgement))?;
            shared.update_snapshot(|snapshot| snapshot.received_control_messages += 1);
        }
        FrameKind::ReturnRequest => {
            let request = decode_transition_request_payload(&frame.payload)?;
            shared
                .inbound
                .push_control(ChannelControlEvent::ReturnRequest(request))?;
            shared.update_snapshot(|snapshot| snapshot.received_control_messages += 1);
        }
        FrameKind::ReturnAcknowledgement => {
            let acknowledgement = decode_transition_acknowledgement_payload(&frame.payload)?;
            shared
                .inbound
                .push_control(ChannelControlEvent::ReturnAcknowledgement(acknowledgement))?;
            shared.update_snapshot(|snapshot| snapshot.received_control_messages += 1);
        }
        FrameKind::InputKey
        | FrameKind::InputMouseMove
        | FrameKind::InputMouseButton
        | FrameKind::InputWheel => {
            handle_received_input_frame(shared, frame)?;
        }
        FrameKind::Close => {
            shared.peer_close_received.store(true, Ordering::Release);
            shared.request_shutdown();
            shared.outbound.wake();
            return Ok(true);
        }
        FrameKind::Hello | FrameKind::HelloAcknowledgement => {
            return Err(invalid_data(
                "cross-screen handshake frame appeared after channel activation",
            ));
        }
    }
    Ok(false)
}

const fn application_queue_capacity(protocol: ApplicationProtocol) -> usize {
    match protocol {
        // Workspace control carries the high-frequency P3 pointer stream as well as
        // handoff and lease messages. A short scheduler stall on the receiver must not
        // overflow the generic eight-message mailbox and tear down the TLS connection.
        ApplicationProtocol::Control => CONTROL_APPLICATION_QUEUE_CAPACITY,
        ApplicationProtocol::Layout
        | ApplicationProtocol::Clipboard
        | ApplicationProtocol::File
        | ApplicationProtocol::PairSwitch => APPLICATION_QUEUE_CAPACITY,
    }
}

fn accept_received_sequence(
    shared: &ChannelShared,
    frame: &Frame,
    expected_sequence: &mut u64,
) -> io::Result<()> {
    shared.verify_active_frame(frame, *expected_sequence)?;
    *expected_sequence = expected_sequence
        .checked_add(1)
        .ok_or_else(|| invalid_data("cross-screen receive sequence overflow"))?;
    shared.update_snapshot(|snapshot| {
        snapshot.received_frames += 1;
        snapshot.last_received_sequence = frame.sequence;
    });
    Ok(())
}

fn handle_received_input_frame(shared: &ChannelShared, frame: &Frame) -> io::Result<()> {
    let input = decode_input_payload(frame.kind, &frame.payload)?;
    if frame.kind == FrameKind::InputMouseMove {
        shared.input_cadence.received.observe();
    }
    let coalesced = shared.inbound.push_input(input)?;
    shared.update_snapshot(|snapshot| {
        snapshot.received_input_events += 1;
        if coalesced {
            snapshot.coalesced_input_mouse_moves += 1;
        }
    });
    Ok(())
}

#[derive(Debug)]
struct FrameRateLimiter {
    window_started: Instant,
    received: u32,
}

impl FrameRateLimiter {
    fn new() -> Self {
        Self {
            window_started: Instant::now(),
            received: 0,
        }
    }

    fn observe(&mut self) -> io::Result<()> {
        if self.window_started.elapsed() >= Duration::from_secs(1) {
            self.window_started = Instant::now();
            self.received = 0;
        }
        self.received = self.received.saturating_add(1);
        if self.received > MAX_RECEIVED_FRAMES_PER_SECOND {
            return Err(invalid_data(
                "cross-screen peer exceeded the input frame rate limit",
            ));
        }
        Ok(())
    }
}

fn mouse_payload(position: NormalizedMousePosition) -> Vec<u8> {
    let mut payload = Vec::with_capacity(4);
    payload.extend_from_slice(&position.x.to_le_bytes());
    payload.extend_from_slice(&position.y.to_le_bytes());
    payload
}

fn reliable_frame(message: ReliableMessage) -> (FrameKind, Vec<u8>) {
    match message {
        ReliableMessage::Pong(nonce) => (FrameKind::Pong, nonce.to_le_bytes().to_vec()),
        ReliableMessage::FileDrag(message) => (
            FrameKind::FileDrag,
            message.encode().expect("validated drag message"),
        ),
        ReliableMessage::ScreenInfo(screen) => (FrameKind::ScreenInfo, screen_info_payload(screen)),
        ReliableMessage::ScreenMillimeters(size) => (
            FrameKind::ScreenMillimeters,
            screen_millimeters_payload(size),
        ),
        ReliableMessage::LayoutInfo(layout) => (FrameKind::LayoutInfo, layout_info_payload(layout)),
        ReliableMessage::BoundaryWorkspace(payload) => (FrameKind::BoundaryWorkspace, payload),
        ReliableMessage::EnterRequest(request) => {
            (FrameKind::EnterRequest, transition_request_payload(request))
        }
        ReliableMessage::EnterAcknowledgement(acknowledgement) => (
            FrameKind::EnterAcknowledgement,
            transition_acknowledgement_payload(acknowledgement),
        ),
        ReliableMessage::ReturnRequest(request) => (
            FrameKind::ReturnRequest,
            transition_request_payload(request),
        ),
        ReliableMessage::ReturnAcknowledgement(acknowledgement) => (
            FrameKind::ReturnAcknowledgement,
            transition_acknowledgement_payload(acknowledgement),
        ),
        ReliableMessage::Input(input) => input_payload(input),
    }
}

fn input_payload(input: ChannelInputEvent) -> (FrameKind, Vec<u8>) {
    let mut payload = Vec::with_capacity(16);
    payload.extend_from_slice(&input.control_epoch.to_le_bytes());
    match input.event {
        RemoteInputEvent::Key {
            physical,
            virtual_key,
            transition,
        } => {
            payload.extend_from_slice(&physical.scan_code.to_le_bytes());
            payload.extend_from_slice(&virtual_key.to_le_bytes());
            payload.push(transition_to_byte(transition));
            payload.push(u8::from(physical.extended));
            payload.extend_from_slice(&[0, 0]);
            (FrameKind::InputKey, payload)
        }
        RemoteInputEvent::MouseMove(position) => {
            payload.extend_from_slice(&position.x.to_le_bytes());
            payload.extend_from_slice(&position.y.to_le_bytes());
            (FrameKind::InputMouseMove, payload)
        }
        RemoteInputEvent::MouseButton { button, transition } => {
            payload.push(mouse_button_to_byte(button));
            payload.push(transition_to_byte(transition));
            (FrameKind::InputMouseButton, payload)
        }
        RemoteInputEvent::Wheel { axis, delta } => {
            payload.push(wheel_axis_to_byte(axis));
            payload.extend_from_slice(&delta.to_le_bytes());
            (FrameKind::InputWheel, payload)
        }
    }
}

fn decode_input_payload(kind: FrameKind, payload: &[u8]) -> io::Result<ChannelInputEvent> {
    if !matches!(
        kind,
        FrameKind::InputKey
            | FrameKind::InputMouseMove
            | FrameKind::InputMouseButton
            | FrameKind::InputWheel
    ) || !kind.valid_payload_length(payload.len())
    {
        return Err(invalid_data("invalid cross-screen input payload length"));
    }
    let control_epoch = read_u64(payload, 0)?;
    let event = match kind {
        FrameKind::InputKey => {
            if payload[14..16] != [0, 0] {
                return Err(invalid_data(
                    "cross-screen input-key reserved bytes must be zero",
                ));
            }
            let extended = match payload[13] {
                0 => false,
                1 => true,
                _ => {
                    return Err(invalid_data(
                        "cross-screen input-key extended flag is invalid",
                    ));
                }
            };
            RemoteInputEvent::Key {
                physical: PhysicalKey {
                    scan_code: read_u16(payload, 8)?,
                    extended,
                },
                virtual_key: read_u16(payload, 10)?,
                transition: transition_from_byte(payload[12])?,
            }
        }
        FrameKind::InputMouseMove => RemoteInputEvent::MouseMove(NormalizedMousePosition {
            x: read_u16(payload, 8)?,
            y: read_u16(payload, 10)?,
        }),
        FrameKind::InputMouseButton => RemoteInputEvent::MouseButton {
            button: mouse_button_from_byte(payload[8])?,
            transition: transition_from_byte(payload[9])?,
        },
        FrameKind::InputWheel => RemoteInputEvent::Wheel {
            axis: wheel_axis_from_byte(payload[8])?,
            delta: i16::from_le_bytes([payload[9], payload[10]]),
        },
        _ => return Err(invalid_data("frame kind is not a cross-screen input event")),
    };
    let input = ChannelInputEvent {
        control_epoch,
        event,
    };
    validate_channel_input_event(input)?;
    Ok(input)
}

fn validate_channel_input_event(input: ChannelInputEvent) -> io::Result<()> {
    if input.control_epoch == 0 {
        return Err(invalid_data(
            "cross-screen input control epoch must be non-zero",
        ));
    }
    if matches!(
        input.event,
        RemoteInputEvent::Key {
            physical: PhysicalKey { scan_code: 0, .. },
            ..
        }
    ) {
        return Err(invalid_data(
            "cross-screen input scan code must be non-zero",
        ));
    }
    Ok(())
}

const fn transition_to_byte(transition: KeyTransition) -> u8 {
    match transition {
        KeyTransition::Down => 1,
        KeyTransition::Up => 2,
    }
}

fn transition_from_byte(value: u8) -> io::Result<KeyTransition> {
    match value {
        1 => Ok(KeyTransition::Down),
        2 => Ok(KeyTransition::Up),
        _ => Err(invalid_data("unknown cross-screen input transition")),
    }
}

const fn mouse_button_to_byte(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 1,
        MouseButton::Right => 2,
        MouseButton::Middle => 3,
        MouseButton::X1 => 4,
        MouseButton::X2 => 5,
    }
}

fn mouse_button_from_byte(value: u8) -> io::Result<MouseButton> {
    match value {
        1 => Ok(MouseButton::Left),
        2 => Ok(MouseButton::Right),
        3 => Ok(MouseButton::Middle),
        4 => Ok(MouseButton::X1),
        5 => Ok(MouseButton::X2),
        _ => Err(invalid_data("unknown cross-screen mouse button")),
    }
}

const fn wheel_axis_to_byte(axis: WheelAxis) -> u8 {
    match axis {
        WheelAxis::Vertical => 1,
        WheelAxis::Horizontal => 2,
    }
}

fn wheel_axis_from_byte(value: u8) -> io::Result<WheelAxis> {
    match value {
        1 => Ok(WheelAxis::Vertical),
        2 => Ok(WheelAxis::Horizontal),
        _ => Err(invalid_data("unknown cross-screen wheel axis")),
    }
}

fn screen_info_payload(screen: PhysicalScreenSize) -> Vec<u8> {
    let mut payload = Vec::with_capacity(8);
    payload.extend_from_slice(&screen.width.to_le_bytes());
    payload.extend_from_slice(&screen.height.to_le_bytes());
    payload
}

fn decode_screen_info_payload(payload: &[u8]) -> io::Result<PhysicalScreenSize> {
    if payload.len() != 8 {
        return Err(invalid_data("invalid cross-screen screen-info payload"));
    }
    PhysicalScreenSize::new(read_u32(payload, 0)?, read_u32(payload, 4)?)
}

fn screen_millimeters_payload(size: Option<ScreenMillimeterSize>) -> Vec<u8> {
    let size = size.unwrap_or(ScreenMillimeterSize {
        width: 0,
        height: 0,
    });
    let mut payload = Vec::with_capacity(8);
    payload.extend_from_slice(&size.width.to_le_bytes());
    payload.extend_from_slice(&size.height.to_le_bytes());
    payload
}

fn decode_screen_millimeters_payload(payload: &[u8]) -> io::Result<Option<ScreenMillimeterSize>> {
    if payload.len() != 8 {
        return Err(invalid_data(
            "invalid cross-screen physical-display payload",
        ));
    }
    let width = read_u32(payload, 0)?;
    let height = read_u32(payload, 4)?;
    if width == 0 && height == 0 {
        return Ok(None);
    }
    ScreenMillimeterSize::new(width, height).map(Some)
}

fn layout_info_payload(layout: LayoutConfiguration) -> Vec<u8> {
    let mut payload = Vec::with_capacity(16);
    payload.extend_from_slice(&layout.revision.to_le_bytes());
    payload.push(placement_to_byte(layout.listener_peer_placement));
    payload.push(u8::from(layout.corner_guard_enabled));
    payload.push(layout.corner_guard_pixels);
    payload.push(u8::from(layout.require_second_push));
    payload.extend_from_slice(&[0; 4]);
    payload
}

fn decode_layout_info_payload(payload: &[u8]) -> io::Result<LayoutConfiguration> {
    if payload.len() != 16
        || read_u64(payload, 0)? == u64::MAX
        || payload[9] > 1
        || payload[10] > 96
        || payload[11] > 1
        || payload[12..].iter().any(|value| *value != 0)
    {
        return Err(invalid_data("invalid cross-screen layout-info payload"));
    }
    Ok(LayoutConfiguration {
        listener_peer_placement: placement_from_byte(payload[8])?,
        revision: read_u64(payload, 0)?,
        corner_guard_enabled: payload[9] != 0,
        corner_guard_pixels: payload[10],
        require_second_push: payload[11] != 0,
    })
}

const fn placement_to_byte(placement: PeerPlacement) -> u8 {
    match placement {
        PeerPlacement::Left => 1,
        PeerPlacement::Right => 2,
        PeerPlacement::Above => 3,
        PeerPlacement::Below => 4,
    }
}

fn placement_from_byte(value: u8) -> io::Result<PeerPlacement> {
    match value {
        1 => Ok(PeerPlacement::Left),
        2 => Ok(PeerPlacement::Right),
        3 => Ok(PeerPlacement::Above),
        4 => Ok(PeerPlacement::Below),
        _ => Err(invalid_data("unknown cross-screen peer placement")),
    }
}

fn transition_request_payload(request: ControlTransitionRequest) -> Vec<u8> {
    let logical_length = request
        .logical_boundary
        .as_ref()
        .map_or(0, |logical| logical.boundary_id.len());
    let mut payload = Vec::with_capacity(23 + logical_length);
    payload.extend_from_slice(&request.control_epoch.to_le_bytes());
    payload.push(boundary_byte(request.departure_boundary));
    payload.push(u8::from(request.logical_boundary.is_some()));
    payload.extend_from_slice(&request.normalized_edge_offset.to_le_bytes());
    if let Some(logical) = request.logical_boundary {
        payload.extend_from_slice(&logical.mapping_revision.to_le_bytes());
        payload.extend_from_slice(&logical.parameter.to_le_bytes());
        payload
            .push(u8::try_from(logical.boundary_id.len()).expect("validated boundary id fits u8"));
        payload.extend_from_slice(logical.boundary_id.as_bytes());
    }
    payload
}

fn decode_transition_request_payload(payload: &[u8]) -> io::Result<ControlTransitionRequest> {
    if payload.len() != 12 && !(24..=87).contains(&payload.len()) {
        return Err(invalid_data(
            "invalid cross-screen transition-request payload",
        ));
    }
    let logical_boundary = match payload[9] {
        0 if payload.len() == 12 => None,
        1 if payload.len() >= 24 => {
            let identifier_length = usize::from(payload[22]);
            if identifier_length == 0
                || identifier_length > 64
                || payload.len() != 23 + identifier_length
            {
                return Err(invalid_data(
                    "invalid logical-boundary transition identifier length",
                ));
            }
            let boundary_id = std::str::from_utf8(&payload[23..])
                .map_err(|_| invalid_data("logical-boundary transition id is not UTF-8"))?
                .to_owned();
            Some(LogicalBoundaryTransition {
                boundary_id,
                mapping_revision: read_u64(payload, 12)?,
                parameter: read_u16(payload, 20)?,
            })
        }
        _ => {
            return Err(invalid_data("invalid cross-screen transition-request mode"));
        }
    };
    let request = ControlTransitionRequest {
        control_epoch: read_u64(payload, 0)?,
        departure_boundary: boundary_from_byte(payload[8])?,
        normalized_edge_offset: read_u16(payload, 10)?,
        logical_boundary,
    };
    validate_transition_request(&request)?;
    Ok(request)
}

fn transition_acknowledgement_payload(acknowledgement: ControlTransitionAck) -> Vec<u8> {
    let mut payload = Vec::with_capacity(12);
    payload.extend_from_slice(&acknowledgement.control_epoch.to_le_bytes());
    payload.push(u8::from(acknowledgement.accepted));
    payload.extend_from_slice(&[0; 3]);
    payload
}

fn decode_transition_acknowledgement_payload(payload: &[u8]) -> io::Result<ControlTransitionAck> {
    if payload.len() != 12 || payload[9..12] != [0; 3] || payload[8] > 1 {
        return Err(invalid_data(
            "invalid cross-screen transition-acknowledgement payload",
        ));
    }
    let acknowledgement = ControlTransitionAck {
        control_epoch: read_u64(payload, 0)?,
        accepted: payload[8] == 1,
    };
    validate_transition_acknowledgement(acknowledgement)?;
    Ok(acknowledgement)
}

fn validate_transition_request(request: &ControlTransitionRequest) -> io::Result<()> {
    if request.control_epoch == 0 {
        return Err(invalid_data("cross-screen control epoch must be non-zero"));
    }
    if let Some(logical) = &request.logical_boundary {
        let identifier = logical.boundary_id.as_bytes();
        if identifier.is_empty()
            || identifier.len() > 64
            || logical.boundary_id.trim() != logical.boundary_id
            || logical.boundary_id.chars().any(char::is_control)
        {
            return Err(invalid_data("logical-boundary transition id is invalid"));
        }
        if logical.mapping_revision == 0 || logical.mapping_revision == u64::MAX {
            return Err(invalid_data(
                "logical-boundary transition revision is invalid",
            ));
        }
    }
    Ok(())
}

fn validate_transition_acknowledgement(acknowledgement: ControlTransitionAck) -> io::Result<()> {
    if acknowledgement.control_epoch == 0 {
        return Err(invalid_data("cross-screen control epoch must be non-zero"));
    }
    Ok(())
}

const fn boundary_byte(boundary: ScreenBoundary) -> u8 {
    match boundary {
        ScreenBoundary::Left => 1,
        ScreenBoundary::Right => 2,
        ScreenBoundary::Top => 3,
        ScreenBoundary::Bottom => 4,
    }
}

fn boundary_from_byte(value: u8) -> io::Result<ScreenBoundary> {
    match value {
        1 => Ok(ScreenBoundary::Left),
        2 => Ok(ScreenBoundary::Right),
        3 => Ok(ScreenBoundary::Top),
        4 => Ok(ScreenBoundary::Bottom),
        _ => Err(invalid_data("unknown cross-screen boundary")),
    }
}

fn decode_mouse_payload(payload: &[u8]) -> io::Result<NormalizedMousePosition> {
    if payload.len() != 4 {
        return Err(invalid_data("invalid normalized mouse payload"));
    }
    Ok(NormalizedMousePosition {
        x: read_u16(payload, 0)?,
        y: read_u16(payload, 2)?,
    })
}

fn is_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    )
}

/// Runs a bounded two-machine channel diagnostic without suppressing or injecting input.
///
/// Both endpoints sample only their current physical primary-screen cursor position and send the
/// newest normalized position. Received positions are validated and counted but never passed to
/// `SendInput`.
///
/// # Errors
///
/// Returns an error for invalid bounds, display queries, trust or TLS failures, protocol setup,
/// cursor sampling, or local queue failures.
pub fn run_channel_diagnostic(
    options: &ChannelDiagnosticOptions,
) -> io::Result<ChannelDiagnosticReport> {
    validate_channel_timeout(options.connect_timeout)?;
    if options.lifetime.is_zero() || options.lifetime > MAX_CHANNEL_LIFETIME {
        return Err(invalid_data(
            "cross-screen channel lifetime must be between 1 millisecond and 300 seconds",
        ));
    }
    let mut channel = match options.role {
        ChannelDiagnosticRole::Listen(address) => CrossScreenChannel::listen_once(
            &options.store,
            options.peer_fingerprint,
            address,
            options.connect_timeout,
        )?,
        ChannelDiagnosticRole::Connect(address) => CrossScreenChannel::connect(
            &options.store,
            options.peer_fingerprint,
            address,
            options.connect_timeout,
        )?,
    };
    let mut screen = PhysicalPrimaryScreen::current()?;
    let started = Instant::now();
    let mut next_screen_refresh = started + Duration::from_secs(1);
    let mut previous = None;
    let mut sampled_mouse_positions = 0_u64;
    let mut sampling_error = None;
    while started.elapsed() < options.lifetime {
        if channel.snapshot().state != ChannelState::Connected {
            break;
        }
        if Instant::now() >= next_screen_refresh {
            match PhysicalPrimaryScreen::current() {
                Ok(current) => screen = current,
                Err(error) => {
                    sampling_error = Some(error);
                    break;
                }
            }
            next_screen_refresh = Instant::now() + Duration::from_secs(1);
        }
        match screen.sample_cursor() {
            Ok(Some(position)) if previous != Some(position) => {
                if let Err(error) = channel.send_mouse_position(position) {
                    if error.kind() != io::ErrorKind::BrokenPipe
                        || channel.snapshot().state == ChannelState::Connected
                    {
                        sampling_error = Some(error);
                    }
                    break;
                }
                previous = Some(position);
                sampled_mouse_positions += 1;
            }
            Ok(_) => {}
            Err(error) => {
                sampling_error = Some(error);
                break;
            }
        }
        std::thread::sleep(
            MOUSE_SAMPLE_INTERVAL.min(options.lifetime.saturating_sub(started.elapsed())),
        );
    }
    channel.stop();
    if let Some(error) = sampling_error {
        return Err(error);
    }
    Ok(ChannelDiagnosticReport {
        sampled_mouse_positions,
        snapshot: channel.snapshot(),
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PhysicalPrimaryScreen {
    width: i32,
    height: i32,
}

impl PhysicalPrimaryScreen {
    pub(super) fn current() -> io::Result<Self> {
        let mut mode = DEVMODEW {
            dmSize: u16::try_from(size_of::<DEVMODEW>()).expect("DEVMODEW size fits in u16"),
            ..DEVMODEW::default()
        };
        // SAFETY: a null device selects the primary display and mode is valid writable storage.
        if !unsafe { EnumDisplaySettingsW(PCWSTR::null(), ENUM_CURRENT_SETTINGS, &raw mut mode) }
            .as_bool()
        {
            return Err(io::Error::last_os_error());
        }
        let width = i32::try_from(mode.dmPelsWidth)
            .map_err(|_| invalid_data("primary display width exceeds i32"))?;
        let height = i32::try_from(mode.dmPelsHeight)
            .map_err(|_| invalid_data("primary display height exceeds i32"))?;
        if width <= 1 || height <= 1 {
            return Err(invalid_data(
                "physical primary display dimensions are unavailable",
            ));
        }
        Ok(Self { width, height })
    }

    pub(super) fn sample_cursor(self) -> io::Result<Option<NormalizedMousePosition>> {
        let mut point = POINT::default();
        // SAFETY: point is valid writable storage and no pointer escapes the call.
        unsafe { GetPhysicalCursorPos(&raw mut point) }
            .map_err(|error| io::Error::other(format!("physical cursor query failed: {error}")))?;
        if point.x < 0 || point.y < 0 || point.x >= self.width || point.y >= self.height {
            return Ok(None);
        }
        Ok(Some(NormalizedMousePosition {
            x: normalize_physical_coordinate(point.x, self.width),
            y: normalize_physical_coordinate(point.y, self.height),
        }))
    }

    pub(super) fn size(self) -> PhysicalScreenSize {
        PhysicalScreenSize {
            width: u32::try_from(self.width).expect("validated physical width is positive"),
            height: u32::try_from(self.height).expect("validated physical height is positive"),
        }
    }
}

pub(super) fn normalize_physical_coordinate(coordinate: i32, extent: i32) -> u16 {
    let clamped = coordinate.clamp(0, extent - 1);
    u16::try_from(i64::from(clamped) * i64::from(u16::MAX) / i64::from(extent - 1))
        .expect("normalized physical coordinate fits in u16")
}

pub(super) fn denormalize_physical_coordinate(coordinate: u16, extent: i32) -> i32 {
    let numerator = i64::from(coordinate) * i64::from(extent - 1) + i64::from(u16::MAX / 2);
    i32::try_from(numerator / i64::from(u16::MAX))
        .expect("denormalized physical coordinate fits in i32")
}

fn validate_channel_timeout(timeout: Duration) -> io::Result<()> {
    if timeout.is_zero() || timeout > Duration::from_mins(5) {
        return Err(invalid_data(
            "cross-screen connection timeout must be between 1 millisecond and 300 seconds",
        ));
    }
    Ok(())
}

fn validate_connect_address(address: SocketAddr) -> io::Result<()> {
    if address.port() == 0 {
        return Err(invalid_data(
            "cross-screen connection port must be non-zero",
        ));
    }
    validate_peer_ip(address.ip())
}

fn validate_listener_address(address: SocketAddr) -> io::Result<()> {
    if address.port() == 0 {
        return Err(invalid_data("cross-screen listener port must be non-zero"));
    }
    if address.ip().is_unspecified() {
        Ok(())
    } else {
        validate_peer_ip(address.ip())
    }
}

fn validate_peer_ip(address: IpAddr) -> io::Result<()> {
    let allowed = match address {
        IpAddr::V4(address) => address.is_loopback() || address.is_private(),
        IpAddr::V6(address) => address.is_loopback() || address.is_unique_local(),
    };
    if !allowed {
        return Err(invalid_data(
            "cross-screen endpoint must use loopback or private unicast",
        ));
    }
    Ok(())
}

fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    bytes.iter().fold(
        String::with_capacity(bytes.len().saturating_mul(2)),
        |mut text, byte| {
            let _ = write!(text, "{byte:02X}");
            text
        },
    )
}

fn read_u16(bytes: &[u8], offset: usize) -> io::Result<u16> {
    let value = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| invalid_data("truncated cross-screen u16 field"))?;
    Ok(u16::from_le_bytes(
        value.try_into().expect("validated u16 slice length"),
    ))
}

fn read_u32(bytes: &[u8], offset: usize) -> io::Result<u32> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| invalid_data("truncated cross-screen u32 field"))?;
    Ok(u32::from_le_bytes(
        value.try_into().expect("validated u32 slice length"),
    ))
}

fn read_u64(bytes: &[u8], offset: usize) -> io::Result<u64> {
    let value = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| invalid_data("truncated cross-screen u64 field"))?;
    Ok(u64::from_le_bytes(
        value.try_into().expect("validated u64 slice length"),
    ))
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn link_warmup_bursts_respect_cooldown_and_keep_warm_needs_both_ends() {
        let start = Instant::now();
        let mut warmup = LinkWarmup::new();
        assert_eq!(warmup.heartbeat_interval(start, true), HEARTBEAT_INTERVAL);

        assert!(warmup.try_start(start));
        assert_eq!(
            warmup.heartbeat_interval(start, false),
            WARMUP_HEARTBEAT_INTERVAL
        );
        assert_eq!(
            warmup.heartbeat_interval(start + WARMUP_DURATION, false),
            HEARTBEAT_INTERVAL
        );
        assert!(
            !warmup.try_start(
                (start + WARMUP_COOLDOWN)
                    .checked_sub(Duration::from_millis(1))
                    .unwrap()
            )
        );
        assert!(warmup.try_start(start + WARMUP_COOLDOWN));

        let later = start + WARMUP_COOLDOWN + WARMUP_DURATION;
        // Keep-warm needs a local request, the peer's advertisement and local permission.
        warmup.keep_warm_requested = true;
        assert_eq!(warmup.heartbeat_interval(later, true), HEARTBEAT_INTERVAL);
        warmup.peer_allowed = true;
        assert_eq!(
            warmup.heartbeat_interval(later, true),
            KEEP_WARM_HEARTBEAT_INTERVAL
        );
        assert_eq!(warmup.heartbeat_interval(later, false), HEARTBEAT_INTERVAL);
    }

    #[test]
    fn heartbeat_request_interrupts_an_idle_writer_wait() {
        let queue = OutboundQueue::default();
        queue.request_heartbeat();
        let started = Instant::now();
        assert!(
            queue
                .pop_until(started + Duration::from_secs(5))
                .unwrap()
                .is_none()
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let mut random = [0_u8; 8];
            getrandom::fill(&mut random).unwrap();
            let path = std::env::temp_dir().join(format!(
                "pierdesk-cross-screen-{label}-{}-{}",
                std::process::id(),
                u64::from_le_bytes(random)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn input_cadence_probe_reports_and_resets_interval_counts() {
        let probe = InputCadenceProbe::default();
        probe.enqueued.observe();
        probe.sent.observe();
        probe.sent.observe();
        probe.received.observe();
        let snapshot = probe.take();
        assert_eq!(snapshot.enqueued.moves, 1);
        assert_eq!(snapshot.sent.moves, 2);
        assert_eq!(snapshot.received.moves, 1);
        assert_eq!(probe.take(), InputCadenceSnapshot::default());
    }

    #[test]
    fn control_cadence_probe_keeps_stage_counts_and_resets_on_new_crossing() {
        let probe = ControlCadenceProbe::default();
        probe.enqueued.observe();
        probe.sent.observe();
        probe.received.observe();
        probe.dequeued.observe();
        let window = probe.take();
        assert_eq!(window.enqueued.moves, 1);
        assert_eq!(window.sent.moves, 1);
        assert_eq!(window.received.moves, 1);
        assert_eq!(window.dequeued.moves, 1);
        assert_eq!(probe.take(), ControlCadenceSnapshot::default());
        probe.enqueued.observe();
        probe.reset();
        probe.enqueued.observe();
        let next = probe.take();
        assert_eq!(next.enqueued.moves, 1);
        assert_eq!(next.enqueued.max_gap_micros, 0);
    }

    #[test]
    fn reader_stage_probe_reports_and_resets_bounded_stage_metrics() {
        let probe = ReaderStageProbe::default();
        probe.socket_read(Duration::from_millis(250), 0, true);
        probe.socket_read(Duration::from_millis(8), 48, false);
        probe.tls_finished(Duration::from_micros(300));
        probe.dispatch_finished(Duration::from_micros(700));
        let window = probe.take();
        assert_eq!(window.socket_reads, 2);
        assert_eq!(window.socket_timeouts, 1);
        assert_eq!(window.socket_bytes, 48);
        assert_eq!(window.socket_wait_max_us, 250_000);
        assert_eq!(window.tls_max_us, 300);
        assert_eq!(window.dispatch_max_us, 700);
        assert_eq!(probe.take(), ReaderStageSnapshot::default());
    }

    #[test]
    fn tcp_path_snapshot_accepts_a_live_loopback_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        let client_info = tcp_path_snapshot(&client).unwrap();
        let server_info = tcp_path_snapshot(&server).unwrap();
        assert!(client_info.receive_window > 0);
        assert!(server_info.receive_window > 0);
        assert_eq!(client_info.pending_bytes, Some(0));
    }

    /// Manual experiment: actual production queue/TLS/socket/decoder, no native input.
    /// Long idle can be selected explicitly without slowing the normal regression suite.
    #[test]
    #[ignore = "manual CF-CS-007 idle/burst timing experiment"]
    fn cf_cs_007_idle_burst_loopback_experiment() {
        let idle_seconds = std::env::var("PIERDESK_EXPERIMENT_IDLE_SECONDS")
            .map_or(2, |value| value.parse::<u64>().unwrap());
        let cycles = std::env::var("PIERDESK_EXPERIMENT_CYCLES")
            .map_or(3, |value| value.parse::<u64>().unwrap());
        assert!((1..=1_800).contains(&idle_seconds));
        assert!((1..=3).contains(&cycles));
        let (_first_dir, first_store, first_id, _second_dir, second_store, second_id) =
            trusted_stores();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            accept_from_listener(&second_store, first_id, &listener, Duration::from_secs(5))
                .unwrap()
        });
        let mut source =
            CrossScreenChannel::connect(&first_store, second_id, address, Duration::from_secs(5))
                .unwrap();
        let mut target = worker.join().unwrap();
        let clock = Instant::now();
        for cycle in 0..cycles {
            std::thread::sleep(Duration::from_secs(idle_seconds));
            source.reset_control_cadence();
            target.reset_control_cadence();
            let mut sent = 0_u64;
            let mut received = 0_u64;
            let mut next_send = Instant::now();
            let burst_start = next_send;
            // Windows scheduling can be coarser than the requested 1 ms sleep.
            // Bound the experiment without mistaking slow sample production for loss.
            let deadline = next_send + Duration::from_secs(30);
            let mut maximum_latency = 0_u64;
            let mut first_latency = 0_u64;
            let mut scheduler_late = Duration::ZERO;
            while received < 500 {
                assert!(
                    Instant::now() < deadline,
                    "bounded burst did not drain: sent={sent}, received={received}"
                );
                let now = Instant::now();
                if sent < 500 && now >= next_send {
                    scheduler_late = scheduler_late.max(now.duration_since(next_send));
                    let mut payload = vec![0x5a; 192];
                    payload[..8].copy_from_slice(&sent.to_le_bytes());
                    payload[8..16].copy_from_slice(&duration_us(clock.elapsed()).to_le_bytes());
                    source
                        .send_application(ApplicationProtocol::Control, payload)
                        .unwrap();
                    sent += 1;
                    next_send += Duration::from_millis(10);
                }
                while let Some(payload) = target
                    .try_receive_application(ApplicationProtocol::Control)
                    .unwrap()
                {
                    assert_eq!(payload.len(), 192);
                    let sequence = u64::from_le_bytes(payload[..8].try_into().unwrap());
                    assert_eq!(sequence, received, "missing or reordered control frame");
                    assert!(payload[16..].iter().all(|value| *value == 0x5a));
                    let queued = u64::from_le_bytes(payload[8..16].try_into().unwrap());
                    let latency = duration_us(clock.elapsed()).saturating_sub(queued);
                    if received == 0 {
                        first_latency = latency;
                    }
                    maximum_latency = maximum_latency.max(latency);
                    received += 1;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            let source_probe = source.take_control_cadence();
            let target_probe = target.take_control_cadence();
            println!(
                "CF_CS_007_LOOPBACK cycle={cycle} idle_seconds={idle_seconds} sent={sent} received={received} first_us={first_latency} max_latency_us={maximum_latency} burst_elapsed_ms={} scheduler_late_us={} source_write_gap_us={} target_read_gap_us={}",
                burst_start.elapsed().as_millis(),
                duration_us(scheduler_late),
                source_probe.sent.max_gap_micros,
                target_probe.received.max_gap_micros,
            );
            assert!(
                maximum_latency < 500_000,
                "local channel exceeded 500 ms; inspect scheduler and channel"
            );
        }
        source.stop();
        target.stop();
    }

    fn sample_frame(kind: FrameKind, sequence: u64) -> Frame {
        Frame {
            kind,
            sender: CertificateFingerprint::from_certificate(b"peer"),
            session_id: [7; 16],
            channel_epoch: 19,
            sequence,
            payload: match kind {
                FrameKind::Application => vec![ApplicationProtocol::Layout as u8, 42],
                FrameKind::FileDrag => DragMessage {
                    id: 1,
                    kind: DragMessage::PREPARE,
                    port: 45239,
                }
                .encode()
                .unwrap(),
                FrameKind::MouseMove => mouse_payload(NormalizedMousePosition { x: 123, y: 456 }),
                FrameKind::Ping | FrameKind::Pong => 99_u64.to_le_bytes().to_vec(),
                FrameKind::ScreenInfo => screen_info_payload(PhysicalScreenSize {
                    width: 3_840,
                    height: 2_160,
                }),
                FrameKind::ScreenMillimeters => {
                    screen_millimeters_payload(Some(ScreenMillimeterSize {
                        width: 597,
                        height: 336,
                    }))
                }
                FrameKind::LayoutInfo => layout_info_payload(LayoutConfiguration {
                    listener_peer_placement: PeerPlacement::Right,
                    revision: 42,
                    corner_guard_enabled: true,
                    corner_guard_pixels: 32,
                    require_second_push: false,
                }),
                FrameKind::BoundaryWorkspace => b"boundary-workspace".to_vec(),
                FrameKind::EnterRequest | FrameKind::ReturnRequest => {
                    transition_request_payload(ControlTransitionRequest {
                        control_epoch: 27,
                        departure_boundary: ScreenBoundary::Right,
                        normalized_edge_offset: 32_768,
                        logical_boundary: None,
                    })
                }
                FrameKind::EnterAcknowledgement | FrameKind::ReturnAcknowledgement => {
                    transition_acknowledgement_payload(ControlTransitionAck {
                        control_epoch: 27,
                        accepted: true,
                    })
                }
                FrameKind::InputKey => {
                    input_payload(ChannelInputEvent {
                        control_epoch: 27,
                        event: RemoteInputEvent::Key {
                            physical: PhysicalKey {
                                scan_code: 0x1e,
                                extended: false,
                            },
                            virtual_key: 0x41,
                            transition: KeyTransition::Down,
                        },
                    })
                    .1
                }
                FrameKind::InputMouseMove => {
                    input_payload(ChannelInputEvent {
                        control_epoch: 27,
                        event: RemoteInputEvent::MouseMove(NormalizedMousePosition {
                            x: 12_345,
                            y: 54_321,
                        }),
                    })
                    .1
                }
                FrameKind::InputMouseButton => {
                    input_payload(ChannelInputEvent {
                        control_epoch: 27,
                        event: RemoteInputEvent::MouseButton {
                            button: MouseButton::Left,
                            transition: KeyTransition::Down,
                        },
                    })
                    .1
                }
                FrameKind::InputWheel => {
                    input_payload(ChannelInputEvent {
                        control_epoch: 27,
                        event: RemoteInputEvent::Wheel {
                            axis: WheelAxis::Vertical,
                            delta: 120,
                        },
                    })
                    .1
                }
                FrameKind::Hello
                | FrameKind::HelloAcknowledgement
                | FrameKind::ReleaseAll
                | FrameKind::Close => Vec::new(),
            },
        }
    }

    fn trusted_stores() -> (
        TestDirectory,
        DeviceStore,
        CertificateFingerprint,
        TestDirectory,
        DeviceStore,
        CertificateFingerprint,
    ) {
        let first_directory = TestDirectory::new("first");
        let second_directory = TestDirectory::new("second");
        let first_store = DeviceStore::new(&first_directory.0);
        let second_store = DeviceStore::new(&second_directory.0);
        let first = first_store.load_or_create_identity().unwrap().identity;
        let second = second_store.load_or_create_identity().unwrap().identity;
        let first_fingerprint = first.fingerprint();
        let second_fingerprint = second.fingerprint();
        first_store
            .trust_peer(
                second.certificate_der().to_vec(),
                second_fingerprint,
                "second",
            )
            .unwrap();
        second_store
            .trust_peer(first.certificate_der().to_vec(), first_fingerprint, "first")
            .unwrap();
        (
            first_directory,
            first_store,
            first_fingerprint,
            second_directory,
            second_store,
            second_fingerprint,
        )
    }

    #[test]
    fn every_protocol_frame_round_trips_exactly() {
        for (index, kind) in [
            FrameKind::Hello,
            FrameKind::HelloAcknowledgement,
            FrameKind::MouseMove,
            FrameKind::Ping,
            FrameKind::Pong,
            FrameKind::ReleaseAll,
            FrameKind::Close,
            FrameKind::ScreenInfo,
            FrameKind::EnterRequest,
            FrameKind::EnterAcknowledgement,
            FrameKind::ReturnRequest,
            FrameKind::ReturnAcknowledgement,
            FrameKind::InputKey,
            FrameKind::InputMouseMove,
            FrameKind::InputMouseButton,
            FrameKind::InputWheel,
            FrameKind::LayoutInfo,
            FrameKind::FileDrag,
            FrameKind::ScreenMillimeters,
            FrameKind::BoundaryWorkspace,
            FrameKind::Application,
        ]
        .into_iter()
        .enumerate()
        {
            let frame = sample_frame(kind, u64::try_from(index + 1).unwrap());
            assert_eq!(Frame::decode(&frame.encode().unwrap()).unwrap(), frame);
        }
    }

    #[test]
    fn protocol_rejects_reserved_unknown_oversized_and_zero_binding_fields() {
        let frame = sample_frame(FrameKind::MouseMove, 2);
        let encoded = frame.encode().unwrap();
        for (offset, value) in [(0, b'X'), (8, 1), (10, 0xff), (11, 1), (14, 1)] {
            let mut malformed = encoded.clone();
            malformed[offset] = value;
            assert!(Frame::decode(&malformed).is_err());
        }
        let mut oversized = encoded.clone();
        oversized[12..14].copy_from_slice(&17_u16.to_le_bytes());
        assert!(Frame::decode(&oversized).is_err());

        let mut zero_session = frame.clone();
        zero_session.session_id = [0; 16];
        assert!(zero_session.encode().is_err());
        let mut zero_epoch = frame.clone();
        zero_epoch.channel_epoch = 0;
        assert!(zero_epoch.encode().is_err());
        let mut zero_sequence = frame;
        zero_sequence.sequence = 0;
        assert!(zero_sequence.encode().is_err());
    }

    #[test]
    fn every_truncated_protocol_frame_is_rejected_without_partial_acceptance() {
        let encoded = sample_frame(FrameKind::MouseMove, 2).encode().unwrap();
        for length in 0..encoded.len() {
            assert!(Frame::decode(&encoded[..length]).is_err());
        }
        assert!(Frame::decode(&encoded).is_ok());
    }

    #[test]
    fn reconnect_bindings_are_nonzero_and_fresh() {
        let first = SessionBinding::generate().unwrap();
        let second = SessionBinding::generate().unwrap();
        assert_ne!(first, second);
        assert!(first.session_id.iter().any(|byte| *byte != 0));
        assert_ne!(first.channel_epoch, 0);
        assert!(second.session_id.iter().any(|byte| *byte != 0));
        assert_ne!(second.channel_epoch, 0);
    }

    #[test]
    fn decoder_handles_fragmentation_and_multiple_frames_without_unbounded_growth() {
        let first = sample_frame(FrameKind::MouseMove, 2);
        let second = sample_frame(FrameKind::Ping, 3);
        let mut combined = first.encode().unwrap();
        combined.extend_from_slice(&second.encode().unwrap());
        let mut decoder = FrameDecoder::default();
        let mut parsed_frames = Vec::new();
        for byte in combined {
            parsed_frames.extend(decoder.push(&[byte]).unwrap());
        }
        assert_eq!(parsed_frames, [first, second]);
        assert!(decoder.buffered.is_empty());
        assert!(decoder.push(&vec![0_u8; MAX_DECODE_BUFFER + 1]).is_err());
    }

    #[test]
    fn application_queues_are_independent_bounded_and_below_input_priority() {
        let queue = OutboundQueue::default();
        for _ in 0..APPLICATION_QUEUE_CAPACITY {
            queue
                .push_application(ApplicationProtocol::File, vec![1; 4096])
                .unwrap();
        }
        assert_eq!(
            queue
                .push_application(ApplicationProtocol::File, vec![2])
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        queue
            .push_application(ApplicationProtocol::Layout, vec![3])
            .unwrap();
        queue
            .push_application(ApplicationProtocol::Control, vec![4])
            .unwrap();
        queue
            .push_reliable(ReliableMessage::ScreenInfo(PhysicalScreenSize {
                width: 1920,
                height: 1080,
            }))
            .unwrap();
        assert!(matches!(
            queue.pop_until(Instant::now()).unwrap(),
            Some(QueueItem::Reliable(_))
        ));
        queue.request_release_all();
        assert_eq!(
            queue.pop_until(Instant::now()).unwrap(),
            Some(QueueItem::ReleaseAll)
        );
        assert_eq!(
            queue.pop_until(Instant::now()).unwrap(),
            Some(QueueItem::Application(ApplicationMessage {
                protocol: ApplicationProtocol::Layout,
                payload: vec![3]
            }))
        );
        assert!(matches!(
            queue.pop_until(Instant::now()).unwrap(),
            Some(QueueItem::Application(ApplicationMessage {
                protocol: ApplicationProtocol::File,
                ..
            }))
        ));
        assert!(
            queue
                .push_application(
                    ApplicationProtocol::Layout,
                    vec![0; MAX_APPLICATION_PAYLOAD]
                )
                .is_err()
        );
        queue.request_shutdown();
        assert_eq!(
            queue.pop_until(Instant::now()).unwrap(),
            Some(QueueItem::Shutdown)
        );
        assert_eq!(queue.pop_until(Instant::now()).unwrap(), None);
    }

    #[test]
    fn full_layout_frame_decodes_across_fragmented_tls_reads_and_rejects_oversize() {
        let mut frame = sample_frame(FrameKind::Application, 2);
        frame.payload = vec![1; 12000];
        let encoded = frame.encode().unwrap();
        let mut decoder = FrameDecoder::default();
        let mut frames = Vec::new();
        for chunk in encoded.chunks(173) {
            frames.extend(decoder.push(chunk).unwrap());
        }
        assert_eq!(frames, vec![frame.clone()]);
        frame.payload.resize(MAX_APPLICATION_PAYLOAD + 1, 0);
        assert!(frame.encode().is_err());
    }

    #[test]
    fn outbound_queue_coalesces_mouse_and_discards_input_before_release_or_shutdown() {
        let queue = OutboundQueue::default();
        assert!(
            !queue
                .push_mouse_move(NormalizedMousePosition { x: 1, y: 2 })
                .unwrap()
        );
        assert!(
            queue
                .push_mouse_move(NormalizedMousePosition { x: 3, y: 4 })
                .unwrap()
        );
        queue.push_reliable(ReliableMessage::Pong(8)).unwrap();
        queue.request_release_all();
        assert_eq!(
            queue.pop_until(Instant::now()).unwrap(),
            Some(QueueItem::ReleaseAll)
        );
        assert_eq!(queue.pop_until(Instant::now()).unwrap(), None);
        queue
            .push_mouse_move(NormalizedMousePosition { x: 5, y: 6 })
            .unwrap();
        queue.request_shutdown();
        assert_eq!(
            queue.pop_until(Instant::now()).unwrap(),
            Some(QueueItem::Shutdown)
        );
        queue.request_shutdown();
        assert_eq!(queue.pop_until(Instant::now()).unwrap(), None);
    }

    #[test]
    fn input_queue_coalesces_consecutive_moves_without_crossing_drag_ordering_barriers() {
        let queue = OutboundQueue::default();
        let epoch = 41;
        let button_down = ChannelInputEvent {
            control_epoch: epoch,
            event: RemoteInputEvent::MouseButton {
                button: MouseButton::Left,
                transition: KeyTransition::Down,
            },
        };
        let first_move = ChannelInputEvent {
            control_epoch: epoch,
            event: RemoteInputEvent::MouseMove(NormalizedMousePosition { x: 10, y: 20 }),
        };
        let latest_move = ChannelInputEvent {
            control_epoch: epoch,
            event: RemoteInputEvent::MouseMove(NormalizedMousePosition { x: 30, y: 40 }),
        };
        let button_up = ChannelInputEvent {
            control_epoch: epoch,
            event: RemoteInputEvent::MouseButton {
                button: MouseButton::Left,
                transition: KeyTransition::Up,
            },
        };
        assert!(!queue.push_input(button_down).unwrap());
        assert!(!queue.push_input(first_move).unwrap());
        assert!(queue.push_input(latest_move).unwrap());
        assert!(!queue.push_input(button_up).unwrap());
        for expected in [button_down, latest_move, button_up] {
            assert_eq!(
                queue.pop_until(Instant::now()).unwrap(),
                Some(QueueItem::Reliable(ReliableMessage::Input(expected)))
            );
        }

        queue.push_input(button_down).unwrap();
        queue.push_input(latest_move).unwrap();
        queue.request_release_all();
        assert_eq!(
            queue.pop_until(Instant::now()).unwrap(),
            Some(QueueItem::ReleaseAll)
        );
        assert_eq!(queue.pop_until(Instant::now()).unwrap(), None);
    }

    #[test]
    fn input_release_preserves_ordered_file_drag_handoff_messages() {
        let queue = OutboundQueue::default();
        let messages = [
            DragMessage::READY,
            DragMessage::ACTIVATE,
            DragMessage::RELEASE,
        ];
        for kind in messages {
            queue
                .push_reliable(ReliableMessage::FileDrag(DragMessage {
                    id: 57,
                    kind,
                    port: 0,
                }))
                .unwrap();
        }
        queue.request_release_all();
        assert_eq!(
            queue.pop_until(Instant::now()).unwrap(),
            Some(QueueItem::ReleaseAll)
        );
        for kind in messages {
            assert_eq!(
                queue.pop_until(Instant::now()).unwrap(),
                Some(QueueItem::Reliable(ReliableMessage::FileDrag(
                    DragMessage {
                        id: 57,
                        kind,
                        port: 0
                    }
                )))
            );
        }
        assert_eq!(queue.pop_until(Instant::now()).unwrap(), None);
    }

    #[test]
    fn every_input_payload_round_trips_and_rejects_invalid_epochs_or_tags() {
        let inputs = [
            ChannelInputEvent {
                control_epoch: 99,
                event: RemoteInputEvent::Key {
                    physical: PhysicalKey {
                        scan_code: 0x1d,
                        extended: true,
                    },
                    virtual_key: 0xa3,
                    transition: KeyTransition::Up,
                },
            },
            ChannelInputEvent {
                control_epoch: 99,
                event: RemoteInputEvent::MouseMove(NormalizedMousePosition {
                    x: 12_345,
                    y: 54_321,
                }),
            },
            ChannelInputEvent {
                control_epoch: 99,
                event: RemoteInputEvent::MouseButton {
                    button: MouseButton::X2,
                    transition: KeyTransition::Down,
                },
            },
            ChannelInputEvent {
                control_epoch: 99,
                event: RemoteInputEvent::Wheel {
                    axis: WheelAxis::Horizontal,
                    delta: -240,
                },
            },
        ];
        for input in inputs {
            let (kind, payload) = input_payload(input);
            assert_eq!(decode_input_payload(kind, &payload).unwrap(), input);
        }

        let (kind, mut payload) = input_payload(inputs[0]);
        payload[..8].fill(0);
        assert!(decode_input_payload(kind, &payload).is_err());
        let (_, mut payload) = input_payload(inputs[0]);
        payload[12] = 9;
        assert!(decode_input_payload(kind, &payload).is_err());
        let (_, mut payload) = input_payload(inputs[0]);
        payload[13] = 2;
        assert!(decode_input_payload(kind, &payload).is_err());
        let (_, mut payload) = input_payload(inputs[0]);
        payload[15] = 1;
        assert!(decode_input_payload(kind, &payload).is_err());
        assert!(decode_input_payload(kind, &payload[..8]).is_err());
    }

    #[test]
    fn reliable_queue_overflow_fails_instead_of_dropping_control_frames() {
        let queue = OutboundQueue::default();
        for nonce in 0..RELIABLE_QUEUE_CAPACITY {
            queue
                .push_reliable(ReliableMessage::Pong(u64::try_from(nonce).unwrap()))
                .unwrap();
        }
        assert!(queue.push_reliable(ReliableMessage::Pong(999)).is_err());
    }

    #[test]
    fn drag_control_wire_rejects_ambiguous_or_unbounded_payloads() {
        for kind in DragMessage::PREPARE..=DragMessage::PREPARE_TUNNEL {
            let message = DragMessage {
                id: 91,
                kind,
                port: if matches!(kind, DragMessage::PREPARE | DragMessage::PREPARE_TUNNEL) {
                    45239
                } else {
                    0
                },
            };
            let payload = message.encode().unwrap();
            assert_eq!(payload.len(), 12);
            assert_eq!(DragMessage::decode(&payload).unwrap(), message);
            assert!(DragMessage::decode(&payload[..11]).is_err());
            let mut reserved = payload.clone();
            reserved[9] = 1;
            assert!(DragMessage::decode(&reserved).is_err());
            assert!(DragMessage { id: 0, ..message }.encode().is_err());
            assert!(DragMessage { kind: 9, ..message }.encode().is_err());
            assert!(
                DragMessage {
                    port: u16::from(!matches!(
                        kind,
                        DragMessage::PREPARE | DragMessage::PREPARE_TUNNEL
                    )),
                    ..message
                }
                .encode()
                .is_err()
            );
        }
    }

    #[test]
    fn inbound_control_queue_overflow_fails_instead_of_dropping_transitions() {
        let queue = InboundEventQueue::default();
        for index in 0..INBOUND_CONTROL_QUEUE_CAPACITY {
            queue
                .push_control(ChannelControlEvent::EnterRequest(
                    ControlTransitionRequest {
                        control_epoch: u64::try_from(index + 1).unwrap(),
                        departure_boundary: ScreenBoundary::Right,
                        normalized_edge_offset: 32_768,
                        logical_boundary: None,
                    },
                ))
                .unwrap();
        }
        assert!(
            queue
                .push_control(ChannelControlEvent::ScreenInfo(PhysicalScreenSize {
                    width: 1_920,
                    height: 1_080,
                }))
                .is_err()
        );
        queue.push_control(ChannelControlEvent::ReleaseAll).unwrap();
        queue.push_control(ChannelControlEvent::ReleaseAll).unwrap();
        assert_eq!(
            queue.pop_control().unwrap(),
            Some(ChannelControlEvent::ReleaseAll)
        );
        assert_eq!(queue.pop_control().unwrap(), None);
    }

    #[test]
    fn file_drag_consumer_preserves_other_control_and_input_events() {
        let queue = InboundEventQueue::default();
        let screen = ChannelControlEvent::ScreenInfo(PhysicalScreenSize {
            width: 1_920,
            height: 1_080,
        });
        let input = ChannelInputEvent {
            control_epoch: 51,
            event: RemoteInputEvent::MouseButton {
                button: MouseButton::Left,
                transition: KeyTransition::Down,
            },
        };
        let drag = DragMessage {
            id: 91,
            kind: DragMessage::ACTIVATE,
            port: 0,
        };
        queue.push_control(screen.clone()).unwrap();
        queue.push_input(input).unwrap();
        queue
            .push_control(ChannelControlEvent::FileDrag(drag))
            .unwrap();

        assert_eq!(queue.pop_file_drag().unwrap(), Some(drag));
        assert_eq!(queue.pop().unwrap(), Some(ChannelEvent::Control(screen)));
        assert_eq!(queue.pop().unwrap(), Some(ChannelEvent::Input(input)));
        assert_eq!(queue.pop().unwrap(), None);
    }

    #[test]
    fn unified_inbound_queue_preserves_control_input_order_and_release_clears_stale_input() {
        let queue = InboundEventQueue::default();
        let enter = ChannelControlEvent::EnterRequest(ControlTransitionRequest {
            control_epoch: 51,
            departure_boundary: ScreenBoundary::Right,
            normalized_edge_offset: 30_000,
            logical_boundary: None,
        });
        let input = ChannelInputEvent {
            control_epoch: 51,
            event: RemoteInputEvent::Key {
                physical: PhysicalKey {
                    scan_code: 0x1e,
                    extended: false,
                },
                virtual_key: 0x41,
                transition: KeyTransition::Down,
            },
        };
        let returned = ChannelControlEvent::ReturnRequest(ControlTransitionRequest {
            control_epoch: 51,
            departure_boundary: ScreenBoundary::Left,
            normalized_edge_offset: 30_000,
            logical_boundary: None,
        });
        queue.push_control(enter.clone()).unwrap();
        queue.push_input(input).unwrap();
        queue.push_control(returned.clone()).unwrap();
        assert_eq!(
            queue.pop().unwrap(),
            Some(ChannelEvent::Control(enter.clone()))
        );
        assert_eq!(queue.pop().unwrap(), Some(ChannelEvent::Input(input)));
        assert_eq!(
            queue.pop().unwrap(),
            Some(ChannelEvent::Control(returned.clone()))
        );

        queue.push_control(enter).unwrap();
        queue.push_input(input).unwrap();
        queue.push_control(returned).unwrap();
        queue.push_control(ChannelControlEvent::ReleaseAll).unwrap();
        assert_eq!(
            queue.pop().unwrap(),
            Some(ChannelEvent::Control(ChannelControlEvent::ReleaseAll))
        );
        assert_eq!(queue.pop().unwrap(), None);
    }

    #[test]
    fn inbound_queue_preserves_every_mouse_sample_in_an_ordinary_network_burst() {
        let queue = InboundEventQueue::default();
        let epoch = 73;
        for coordinate in 0..32_u16 {
            let input = ChannelInputEvent {
                control_epoch: epoch,
                event: RemoteInputEvent::MouseMove(NormalizedMousePosition {
                    x: coordinate,
                    y: coordinate.saturating_add(1),
                }),
            };
            assert!(!queue.push_input(input).unwrap());
        }

        for coordinate in 0..32_u16 {
            assert_eq!(
                queue.pop_input().unwrap(),
                Some(ChannelInputEvent {
                    control_epoch: epoch,
                    event: RemoteInputEvent::MouseMove(NormalizedMousePosition {
                        x: coordinate,
                        y: coordinate.saturating_add(1),
                    }),
                })
            );
        }
        assert_eq!(queue.pop_input().unwrap(), None);
    }

    #[test]
    fn inbound_queue_sheds_only_mouse_state_at_real_capacity_pressure() {
        let queue = InboundEventQueue::default();
        let epoch = 79;
        for coordinate in 0..INBOUND_INPUT_QUEUE_CAPACITY {
            queue
                .push_input(ChannelInputEvent {
                    control_epoch: epoch,
                    event: RemoteInputEvent::MouseMove(NormalizedMousePosition {
                        x: u16::try_from(coordinate).unwrap(),
                        y: 0,
                    }),
                })
                .unwrap();
        }
        let latest = ChannelInputEvent {
            control_epoch: epoch,
            event: RemoteInputEvent::MouseMove(NormalizedMousePosition {
                x: u16::MAX,
                y: u16::MAX,
            }),
        };
        assert!(queue.push_input(latest).unwrap());
        let release = ChannelInputEvent {
            control_epoch: epoch,
            event: RemoteInputEvent::MouseButton {
                button: MouseButton::Left,
                transition: KeyTransition::Up,
            },
        };
        assert!(queue.push_input(release).unwrap());

        for coordinate in 1..(INBOUND_INPUT_QUEUE_CAPACITY - 1) {
            assert_eq!(
                queue.pop_input().unwrap(),
                Some(ChannelInputEvent {
                    control_epoch: epoch,
                    event: RemoteInputEvent::MouseMove(NormalizedMousePosition {
                        x: u16::try_from(coordinate).unwrap(),
                        y: 0,
                    }),
                })
            );
        }
        assert_eq!(queue.pop_input().unwrap(), Some(latest));
        assert_eq!(queue.pop_input().unwrap(), Some(release));
        assert_eq!(queue.pop_input().unwrap(), None);
    }

    #[test]
    fn control_payloads_reject_zero_epochs_unknown_values_and_nonzero_reserved_bytes() {
        let request = ControlTransitionRequest {
            control_epoch: 71,
            departure_boundary: ScreenBoundary::Bottom,
            normalized_edge_offset: 42_000,
            logical_boundary: None,
        };
        assert_eq!(
            decode_transition_request_payload(&transition_request_payload(request.clone()))
                .unwrap(),
            request
        );
        let acknowledgement = ControlTransitionAck {
            control_epoch: 71,
            accepted: true,
        };
        assert_eq!(
            decode_transition_acknowledgement_payload(&transition_acknowledgement_payload(
                acknowledgement
            ))
            .unwrap(),
            acknowledgement
        );
        let mut malformed_request = transition_request_payload(request.clone());
        malformed_request[8] = 9;
        assert!(decode_transition_request_payload(&malformed_request).is_err());
        malformed_request = transition_request_payload(request);
        malformed_request[9] = 1;
        assert!(decode_transition_request_payload(&malformed_request).is_err());
        malformed_request[..8].fill(0);
        assert!(decode_transition_request_payload(&malformed_request).is_err());

        let mut malformed_acknowledgement = transition_acknowledgement_payload(acknowledgement);
        malformed_acknowledgement[8] = 2;
        assert!(decode_transition_acknowledgement_payload(&malformed_acknowledgement).is_err());
        malformed_acknowledgement = transition_acknowledgement_payload(acknowledgement);
        malformed_acknowledgement[11] = 1;
        assert!(decode_transition_acknowledgement_payload(&malformed_acknowledgement).is_err());

        assert!(
            decode_screen_info_payload(&screen_info_payload(PhysicalScreenSize {
                width: 1,
                height: 1_080,
            }))
            .is_err()
        );

        let millimeters = ScreenMillimeterSize::new(597, 336).unwrap();
        assert_eq!(
            decode_screen_millimeters_payload(&screen_millimeters_payload(Some(millimeters)))
                .unwrap(),
            Some(millimeters)
        );
        assert_eq!(
            decode_screen_millimeters_payload(&screen_millimeters_payload(None)).unwrap(),
            None
        );
        for (width, height) in [(0_u32, 336_u32), (597, 0), (10_001, 336), (597, 10_001)] {
            let mut payload = Vec::with_capacity(8);
            payload.extend_from_slice(&width.to_le_bytes());
            payload.extend_from_slice(&height.to_le_bytes());
            assert!(decode_screen_millimeters_payload(&payload).is_err());
        }

        let layout = LayoutConfiguration {
            listener_peer_placement: PeerPlacement::Below,
            revision: 99,
            corner_guard_enabled: true,
            corner_guard_pixels: 48,
            require_second_push: true,
        };
        let payload = layout_info_payload(layout);
        assert_eq!(decode_layout_info_payload(&payload).unwrap(), layout);
        for (index, value) in [(8, 9), (9, 2), (10, 97), (11, 2), (15, 1)] {
            let mut malformed = payload.clone();
            malformed[index] = value;
            assert!(decode_layout_info_payload(&malformed).is_err());
        }
    }

    #[test]
    fn logical_boundary_transition_payload_round_trips_and_rejects_malformed_metadata() {
        let request = ControlTransitionRequest {
            control_epoch: 72,
            departure_boundary: ScreenBoundary::Top,
            normalized_edge_offset: 21_000,
            logical_boundary: Some(LogicalBoundaryTransition {
                boundary_id: "desk-a-to-b".to_owned(),
                mapping_revision: 19,
                parameter: 48_000,
            }),
        };
        let payload = transition_request_payload(request.clone());
        assert_eq!(
            decode_transition_request_payload(&payload).unwrap(),
            request
        );
        assert!(FrameKind::EnterRequest.valid_payload_length(payload.len()));

        let mut zero_revision = payload.clone();
        zero_revision[12..20].fill(0);
        assert!(decode_transition_request_payload(&zero_revision).is_err());

        let mut missing_identifier = payload.clone();
        missing_identifier[22] = 0;
        assert!(decode_transition_request_payload(&missing_identifier).is_err());

        let mut invalid_utf8 = payload;
        invalid_utf8[23] = 0xff;
        assert!(decode_transition_request_payload(&invalid_utf8).is_err());
    }

    #[test]
    fn normalized_coordinates_cover_resolution_and_dpi_independent_endpoints() {
        for extent in [768, 1_080, 1_440, 2_160, 2_880, 4_320] {
            assert_eq!(normalize_physical_coordinate(0, extent), 0);
            assert_eq!(normalize_physical_coordinate(extent - 1, extent), u16::MAX);
            let midpoint = normalize_physical_coordinate(extent / 2, extent);
            let recovered = i32::try_from(
                (u64::from(midpoint) * u64::try_from(extent - 1).unwrap()
                    + u64::from(u16::MAX / 2))
                    / u64::from(u16::MAX),
            )
            .unwrap();
            assert!(recovered.abs_diff(extent / 2) <= 1);
        }
    }

    #[test]
    fn frame_rate_limit_is_fixed_and_enforced() {
        let mut limiter = FrameRateLimiter::new();
        for _ in 0..MAX_RECEIVED_FRAMES_PER_SECOND {
            limiter.observe().unwrap();
        }
        assert!(limiter.observe().is_err());
    }

    fn exchange_sample_control_frames(client: &CrossScreenChannel, server: &CrossScreenChannel) {
        client
            .send_screen_info(PhysicalScreenSize {
                width: 3_840,
                height: 2_160,
            })
            .unwrap();
        client
            .send_screen_millimeters(Some(ScreenMillimeterSize {
                width: 597,
                height: 336,
            }))
            .unwrap();
        client
            .send_enter_request(ControlTransitionRequest {
                control_epoch: 51,
                departure_boundary: ScreenBoundary::Right,
                normalized_edge_offset: 25_000,
                logical_boundary: None,
            })
            .unwrap();
        server
            .send_screen_info(PhysicalScreenSize {
                width: 2_560,
                height: 1_440,
            })
            .unwrap();
        server.send_screen_millimeters(None).unwrap();
        server
            .send_enter_acknowledgement(ControlTransitionAck {
                control_epoch: 51,
                accepted: true,
            })
            .unwrap();
        server
            .send_return_request(ControlTransitionRequest {
                control_epoch: 51,
                departure_boundary: ScreenBoundary::Left,
                normalized_edge_offset: 31_000,
                logical_boundary: None,
            })
            .unwrap();
        client
            .send_return_acknowledgement(ControlTransitionAck {
                control_epoch: 51,
                accepted: true,
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while (client.snapshot().received_control_messages != 4
            || server.snapshot().received_control_messages != 4)
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_sample_server_control_events(server);
        assert_sample_client_control_events(client);
    }

    fn assert_sample_server_control_events(server: &CrossScreenChannel) {
        assert_eq!(
            server.try_receive_control_event().unwrap(),
            Some(ChannelControlEvent::ScreenInfo(PhysicalScreenSize {
                width: 3_840,
                height: 2_160,
            }))
        );
        assert_eq!(
            server.try_receive_control_event().unwrap(),
            Some(ChannelControlEvent::ScreenMillimeters(Some(
                ScreenMillimeterSize {
                    width: 597,
                    height: 336,
                }
            )))
        );
        assert_eq!(
            server.try_receive_control_event().unwrap(),
            Some(ChannelControlEvent::EnterRequest(
                ControlTransitionRequest {
                    control_epoch: 51,
                    departure_boundary: ScreenBoundary::Right,
                    normalized_edge_offset: 25_000,
                    logical_boundary: None,
                }
            ))
        );
        assert_eq!(
            server.try_receive_control_event().unwrap(),
            Some(ChannelControlEvent::ReturnAcknowledgement(
                ControlTransitionAck {
                    control_epoch: 51,
                    accepted: true,
                }
            ))
        );
        assert_eq!(server.try_receive_control_event().unwrap(), None);
    }

    fn assert_sample_client_control_events(client: &CrossScreenChannel) {
        assert_eq!(
            client.try_receive_control_event().unwrap(),
            Some(ChannelControlEvent::ScreenInfo(PhysicalScreenSize {
                width: 2_560,
                height: 1_440,
            }))
        );
        assert_eq!(
            client.try_receive_control_event().unwrap(),
            Some(ChannelControlEvent::ScreenMillimeters(None))
        );
        assert_eq!(
            client.try_receive_control_event().unwrap(),
            Some(ChannelControlEvent::EnterAcknowledgement(
                ControlTransitionAck {
                    control_epoch: 51,
                    accepted: true,
                }
            ))
        );
        assert_eq!(
            client.try_receive_control_event().unwrap(),
            Some(ChannelControlEvent::ReturnRequest(
                ControlTransitionRequest {
                    control_epoch: 51,
                    departure_boundary: ScreenBoundary::Left,
                    normalized_edge_offset: 31_000,
                    logical_boundary: None,
                }
            ))
        );
        assert_eq!(client.try_receive_control_event().unwrap(), None);
    }

    fn assert_active_binding_rejects_mismatches(
        client: &CrossScreenChannel,
        first_fingerprint: CertificateFingerprint,
        second_fingerprint: CertificateFingerprint,
    ) {
        let wrong_binding = Frame {
            kind: FrameKind::MouseMove,
            sender: second_fingerprint,
            session_id: [9; 16],
            channel_epoch: client.shared.binding.channel_epoch,
            sequence: 2,
            payload: mouse_payload(NormalizedMousePosition { x: 0, y: 0 }),
        };
        assert!(
            client
                .shared
                .verify_active_frame(&wrong_binding, 2)
                .is_err()
        );
        let mut wrong_sender = wrong_binding.clone();
        wrong_sender.session_id = client.shared.binding.session_id;
        wrong_sender.sender = first_fingerprint;
        assert!(client.shared.verify_active_frame(&wrong_sender, 2).is_err());
        let mut wrong_sequence = wrong_binding;
        wrong_sequence.session_id = client.shared.binding.session_id;
        wrong_sequence.sequence = 3;
        assert!(
            client
                .shared
                .verify_active_frame(&wrong_sequence, 2)
                .is_err()
        );
    }

    #[test]
    fn pinned_tls_channel_is_bidirectional_sequenced_and_closes_cleanly() {
        let (
            _first_directory,
            first_store,
            first_fingerprint,
            _second_directory,
            second_store,
            second_fingerprint,
        ) = trusted_stores();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            accept_from_listener(
                &second_store,
                first_fingerprint,
                &listener,
                Duration::from_secs(3),
            )
            .unwrap()
        });
        let mut client = CrossScreenChannel::connect(
            &first_store,
            second_fingerprint,
            address,
            Duration::from_secs(3),
        )
        .unwrap();
        let mut server = server.join().unwrap();
        client
            .send_mouse_position(NormalizedMousePosition { x: 100, y: 200 })
            .unwrap();
        server
            .send_mouse_position(NormalizedMousePosition { x: 300, y: 400 })
            .unwrap();
        exchange_sample_control_frames(&client, &server);
        client.request_release_all();

        let deadline = Instant::now() + Duration::from_secs(3);
        while (client.snapshot().received_mouse_moves == 0
            || server.snapshot().received_mouse_moves == 0
            || server.snapshot().received_release_all == 0)
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(client.snapshot().session_id, server.snapshot().session_id);
        assert_eq!(
            client.snapshot().channel_epoch,
            server.snapshot().channel_epoch
        );
        assert_eq!(client.snapshot().received_mouse_moves, 1);
        assert_eq!(server.snapshot().received_mouse_moves, 1);
        assert_eq!(server.snapshot().received_release_all, 1);
        let reader = server.take_reader_stage();
        assert!(reader.socket_reads > 0);
        assert!(reader.socket_bytes > 0);
        assert!(server.tcp_path_snapshot().unwrap().bytes_in > 0);

        assert_active_binding_rejects_mismatches(&client, first_fingerprint, second_fingerprint);

        client.stop();
        server.stop();
        let client_snapshot = client.snapshot();
        let server_snapshot = server.snapshot();
        assert!(client_snapshot.clean_shutdown);
        assert!(server_snapshot.clean_shutdown);
        assert_eq!(client_snapshot.state, ChannelState::Stopped);
        assert_eq!(server_snapshot.state, ChannelState::Stopped);
        assert_eq!(client_snapshot.sent_release_all, 2);
        assert_eq!(server_snapshot.sent_release_all, 1);
        assert_eq!(client_snapshot.received_release_all, 1);
        assert_eq!(server_snapshot.received_release_all, 2);
    }

    #[test]
    fn input_channel_stays_responsive_during_independent_bulk_transfer() {
        const BULK_BYTES: usize = 16 * 1024 * 1024;
        const CHUNK_BYTES: usize = 64 * 1024;
        const INPUT_EVENTS: u16 = 512;
        let (
            _first_directory,
            first_store,
            first_fingerprint,
            _second_directory,
            second_store,
            second_fingerprint,
        ) = trusted_stores();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            accept_from_listener(
                &second_store,
                first_fingerprint,
                &listener,
                Duration::from_secs(3),
            )
            .unwrap()
        });
        let mut client = CrossScreenChannel::connect(
            &first_store,
            second_fingerprint,
            address,
            Duration::from_secs(3),
        )
        .unwrap();
        let mut server = server.join().unwrap();

        let bulk_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let bulk_address = bulk_listener.local_addr().unwrap();
        let bulk_receiver = std::thread::spawn(move || {
            let (mut stream, _) = bulk_listener.accept().unwrap();
            let mut buffer = vec![0_u8; CHUNK_BYTES];
            let mut received = 0_usize;
            loop {
                let length = stream.read(&mut buffer).unwrap();
                if length == 0 {
                    break;
                }
                received += length;
                std::thread::sleep(Duration::from_millis(1));
            }
            received
        });
        let bulk_sender = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(bulk_address).unwrap();
            let chunk = vec![0x5a_u8; CHUNK_BYTES];
            for _ in 0..(BULK_BYTES / CHUNK_BYTES) {
                stream.write_all(&chunk).unwrap();
            }
            stream.shutdown(Shutdown::Write).unwrap();
        });

        let mut maximum_input_round_trip = Duration::ZERO;
        for index in 0..INPUT_EVENTS {
            let started = Instant::now();
            let expected = ChannelInputEvent {
                control_epoch: 123,
                event: RemoteInputEvent::Wheel {
                    axis: if index.is_multiple_of(2) {
                        WheelAxis::Vertical
                    } else {
                        WheelAxis::Horizontal
                    },
                    delta: if index.is_multiple_of(2) { 120 } else { -120 },
                },
            };
            client.send_input_event(expected).unwrap();
            let deadline = started + Duration::from_secs(2);
            loop {
                if let Some(received) = server.try_receive_input_event().unwrap() {
                    assert_eq!(received, expected);
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "input event was blocked by bulk transfer"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            maximum_input_round_trip = maximum_input_round_trip.max(started.elapsed());
        }
        bulk_sender.join().unwrap();
        assert_eq!(bulk_receiver.join().unwrap(), BULK_BYTES);
        assert!(maximum_input_round_trip < Duration::from_secs(2));
        assert_eq!(
            server.snapshot().received_input_events,
            u64::from(INPUT_EVENTS)
        );

        client.stop();
        server.stop();
        assert!(client.snapshot().clean_shutdown);
        assert!(server.snapshot().clean_shutdown);
    }

    #[test]
    fn simultaneous_graceful_shutdown_is_clean_on_both_endpoints() {
        let (
            _first_directory,
            first_store,
            first_fingerprint,
            _second_directory,
            second_store,
            second_fingerprint,
        ) = trusted_stores();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            accept_from_listener(
                &second_store,
                first_fingerprint,
                &listener,
                Duration::from_secs(3),
            )
            .unwrap()
        });
        let client = CrossScreenChannel::connect(
            &first_store,
            second_fingerprint,
            address,
            Duration::from_secs(3),
        )
        .unwrap();
        let server = server.join().unwrap();
        let client_stop = std::thread::spawn(move || {
            let mut client = client;
            client.stop();
            client.snapshot()
        });
        let server_stop = std::thread::spawn(move || {
            let mut server = server;
            server.stop();
            server.snapshot()
        });
        let client_snapshot = client_stop.join().unwrap();
        let server_snapshot = server_stop.join().unwrap();
        assert!(client_snapshot.clean_shutdown);
        assert!(server_snapshot.clean_shutdown);
        assert_eq!(client_snapshot.state, ChannelState::Stopped);
        assert_eq!(server_snapshot.state, ChannelState::Stopped);
        assert_eq!(client_snapshot.sent_release_all, 1);
        assert_eq!(server_snapshot.sent_release_all, 1);
        assert_eq!(client_snapshot.received_release_all, 1);
        assert_eq!(server_snapshot.received_release_all, 1);
    }

    #[test]
    fn abrupt_tls_disconnect_is_failed_not_reported_as_clean() {
        let (
            _first_directory,
            first_store,
            first_fingerprint,
            _second_directory,
            second_store,
            second_fingerprint,
        ) = trusted_stores();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            accept_from_listener(
                &second_store,
                first_fingerprint,
                &listener,
                Duration::from_secs(3),
            )
            .unwrap()
        });
        let mut client = CrossScreenChannel::connect(
            &first_store,
            second_fingerprint,
            address,
            Duration::from_secs(3),
        )
        .unwrap();
        let mut server = server.join().unwrap();
        client
            .shared
            .shutdown_socket
            .shutdown(Shutdown::Both)
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while (client.snapshot().state != ChannelState::Failed
            || server.snapshot().state != ChannelState::Failed)
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        client.stop();
        server.stop();
        assert_eq!(client.snapshot().state, ChannelState::Failed);
        assert_eq!(server.snapshot().state, ChannelState::Failed);
        assert!(!client.snapshot().clean_shutdown);
        assert!(!server.snapshot().clean_shutdown);
    }
}
