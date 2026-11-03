use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::cross_screen::{
    BoundaryLayoutV3, BoundarySyncPhase, BoundarySyncSurface, SynchronizedScreenCatalog,
    SynchronizedScreenInfo, encode_boundary_sync_with_initiator,
};
use crate::device_store::DeviceStore;
use crate::discovery::DiscoveryView;
use crate::security::{CertificateFingerprint, PinnedTlsClient};

pub(crate) const SCREEN_INFO_MAGIC: &[u8; 8] = b"CFSCRN01";
const SCREEN_INFO_ACK: &[u8; 8] = b"CFSCRAK1";
const RESPONSE_LENGTH: usize = 8 + 4 * 4;
const PROBE_TIMEOUT: Duration = Duration::from_millis(750);
const SUCCESS_REFRESH: Duration = Duration::from_secs(30);
const INITIAL_RETRY: Duration = Duration::from_secs(2);
const MAX_RETRY: Duration = Duration::from_secs(30);
const LOOP_DELAY: Duration = Duration::from_millis(200);
const BOUNDARY_RETRY: Duration = Duration::from_millis(500);

#[derive(Clone, Copy)]
struct ProbeState {
    next_attempt: Instant,
    retry_delay: Duration,
    sent_boundary_generation: u64,
    next_boundary_attempt: Instant,
}

#[derive(Clone)]
struct OutboundBoundaryWorkspace {
    generation: u64,
    layout: BoundaryLayoutV3,
    phase: BoundarySyncPhase,
    surface: BoundarySyncSurface,
    initiator: CertificateFingerprint,
}

#[derive(Default)]
struct BoundaryOutbox {
    generation: u64,
    latest_by_peer: HashMap<CertificateFingerprint, OutboundBoundaryWorkspace>,
}

impl BoundaryOutbox {
    fn queue_for_peer(
        &mut self,
        peer: CertificateFingerprint,
        layout: &BoundaryLayoutV3,
        phase: BoundarySyncPhase,
        surface: BoundarySyncSurface,
        initiator: CertificateFingerprint,
    ) {
        self.generation = self.generation.wrapping_add(1).max(1);
        self.latest_by_peer.insert(
            peer,
            OutboundBoundaryWorkspace {
                generation: self.generation,
                layout: layout.clone(),
                phase,
                surface,
                initiator,
            },
        );
    }

    fn queue_for_layout(
        &mut self,
        layout: &BoundaryLayoutV3,
        phase: BoundarySyncPhase,
        surface: BoundarySyncSurface,
        initiator: CertificateFingerprint,
    ) {
        let peers = layout
            .screens
            .iter()
            .filter(|screen| screen.device_fingerprint != layout.local_fingerprint)
            .filter_map(|screen| CertificateFingerprint::from_str(&screen.device_fingerprint).ok())
            .collect::<Vec<_>>();
        for peer in peers {
            self.queue_for_peer(peer, layout, phase, surface, initiator);
        }
    }
}

/// Fetches primary-screen metadata from every online trusted peer over short-lived pinned
/// mutual-TLS connections. This stays independent from the single active control session.
pub(crate) struct ScreenInfoSyncRuntime {
    stop: Arc<AtomicBool>,
    boundary_outbox: Arc<Mutex<BoundaryOutbox>>,
    worker: Option<JoinHandle<()>>,
}

impl ScreenInfoSyncRuntime {
    pub(crate) fn start(store: DeviceStore, discovery: DiscoveryView) -> io::Result<Self> {
        // Fail before starting the worker if the local identity is unavailable. Peer records are
        // loaded inside the loop so newly paired devices synchronize without restarting it.
        store.load_identity()?;
        let stop = Arc::new(AtomicBool::new(false));
        let boundary_outbox = Arc::new(Mutex::new(BoundaryOutbox::default()));
        let worker_stop = Arc::clone(&stop);
        let worker_outbox = Arc::clone(&boundary_outbox);
        let worker = std::thread::Builder::new()
            .name("pierdesk-screen-info-sync".to_owned())
            .spawn(move || run_sync_loop(&store, &discovery, &worker_stop, &worker_outbox))?;
        Ok(Self {
            stop,
            boundary_outbox,
            worker: Some(worker),
        })
    }

    /// Queues the newest user-driven workspace for every online trusted peer in that workspace.
    /// A newer drag preview replaces an older unsent one.
    pub(crate) fn queue_boundary_workspace(
        &self,
        layout: &BoundaryLayoutV3,
        phase: BoundarySyncPhase,
        surface: BoundarySyncSurface,
        initiator: CertificateFingerprint,
    ) {
        let mut outbox = self
            .boundary_outbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        outbox.queue_for_layout(layout, phase, surface, initiator);
    }

    /// Queues a pair-scoped confirmation or applied snapshot without replacing pending work for
    /// any other peer.
    pub(crate) fn queue_boundary_workspace_for_peer(
        &self,
        peer: CertificateFingerprint,
        layout: &BoundaryLayoutV3,
        phase: BoundarySyncPhase,
        surface: BoundarySyncSurface,
        initiator: CertificateFingerprint,
    ) {
        let mut outbox = self
            .boundary_outbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        outbox.queue_for_peer(peer, layout, phase, surface, initiator);
    }

    pub(crate) fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for ScreenInfoSyncRuntime {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_sync_loop(
    store: &DeviceStore,
    discovery: &DiscoveryView,
    stop: &AtomicBool,
    boundary_outbox: &Mutex<BoundaryOutbox>,
) {
    let mut states = HashMap::<CertificateFingerprint, ProbeState>::new();
    while !stop.load(Ordering::Acquire) {
        let now = Instant::now();
        let peers = discovery.peers();
        let outbound_by_peer = boundary_outbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .latest_by_peer
            .clone();
        states.retain(|fingerprint, _| peers.iter().any(|peer| peer.fingerprint == *fingerprint));
        for peer in peers {
            if stop.load(Ordering::Acquire) || store.load_peer(peer.fingerprint).is_err() {
                continue;
            }
            let state = states.entry(peer.fingerprint).or_insert_with(|| {
                let cached = SynchronizedScreenCatalog::load_or_default(store)
                    .ok()
                    .and_then(|catalog| catalog.get(peer.fingerprint))
                    .is_some();
                ProbeState {
                    next_attempt: if cached {
                        now.checked_add(SUCCESS_REFRESH).unwrap_or(now)
                    } else {
                        now
                    },
                    retry_delay: INITIAL_RETRY,
                    sent_boundary_generation: 0,
                    next_boundary_attempt: now,
                }
            });
            if let Some(outbound) = outbound_by_peer.get(&peer.fingerprint)
                && outbound.generation != state.sent_boundary_generation
                && now >= state.next_boundary_attempt
            {
                match encode_boundary_sync_with_initiator(
                    &outbound.layout,
                    peer.fingerprint,
                    outbound.phase,
                    outbound.surface,
                    outbound.initiator,
                ) {
                    Ok(Some(payload)) => {
                        if send_peer_boundary_workspace(
                            store,
                            peer.service_endpoint,
                            peer.fingerprint,
                            &payload,
                        )
                        .is_ok()
                        {
                            state.sent_boundary_generation = outbound.generation;
                        } else {
                            state.next_boundary_attempt = Instant::now()
                                .checked_add(BOUNDARY_RETRY)
                                .unwrap_or_else(Instant::now);
                        }
                    }
                    Ok(None) | Err(_) => {
                        state.sent_boundary_generation = outbound.generation;
                    }
                }
            }
            if now < state.next_attempt {
                continue;
            }
            if let Ok(info) = query_peer_screen(store, peer.service_endpoint, peer.fingerprint) {
                let _ = SynchronizedScreenCatalog::remember_latest(store, peer.fingerprint, info);
                state.next_attempt = Instant::now()
                    .checked_add(SUCCESS_REFRESH)
                    .unwrap_or_else(Instant::now);
                state.retry_delay = INITIAL_RETRY;
            } else {
                state.next_attempt = Instant::now()
                    .checked_add(state.retry_delay)
                    .unwrap_or_else(Instant::now);
                state.retry_delay = state.retry_delay.saturating_mul(2).min(MAX_RETRY);
            }
        }
        sleep_until_stopped(stop, LOOP_DELAY);
    }
}

fn send_peer_boundary_workspace(
    store: &DeviceStore,
    endpoint: SocketAddr,
    fingerprint: CertificateFingerprint,
    payload: &[u8],
) -> io::Result<()> {
    let length = u32::try_from(payload.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "boundary workspace payload is too large",
        )
    })?;
    if payload.is_empty() || payload.len() > crate::clipboard::BOUNDARY_WORKSPACE_PAYLOAD_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "boundary workspace payload is outside its fixed limit",
        ));
    }
    let identity = store.load_identity()?;
    let peer = store.load_peer(fingerprint)?;
    let tls = PinnedTlsClient::new(
        &identity,
        peer.into_certificate_der(),
        fingerprint,
        PROBE_TIMEOUT,
    )?;
    let mut stream = tls.connect_with_timeout(endpoint, PROBE_TIMEOUT)?;
    stream.write_all(crate::clipboard::BOUNDARY_WORKSPACE_MAGIC)?;
    stream.write_all(&length.to_be_bytes())?;
    stream.write_all(payload)?;
    stream.flush()?;
    let mut acknowledgement = [0_u8; 8];
    stream.read_exact(&mut acknowledgement)?;
    if acknowledgement != *crate::clipboard::BOUNDARY_WORKSPACE_ACK {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "peer returned an invalid boundary workspace acknowledgement",
        ));
    }
    let _ = stream.sock.shutdown(Shutdown::Both);
    Ok(())
}

fn query_peer_screen(
    store: &DeviceStore,
    endpoint: SocketAddr,
    fingerprint: CertificateFingerprint,
) -> io::Result<SynchronizedScreenInfo> {
    let identity = store.load_identity()?;
    let peer = store.load_peer(fingerprint)?;
    let tls = PinnedTlsClient::new(
        &identity,
        peer.into_certificate_der(),
        fingerprint,
        PROBE_TIMEOUT,
    )?;
    let mut stream = tls.connect_with_timeout(endpoint, PROBE_TIMEOUT)?;
    stream.write_all(SCREEN_INFO_MAGIC)?;
    stream.flush()?;
    let info = read_screen_info_response(&mut stream)?;
    let _ = stream.sock.shutdown(Shutdown::Both);
    Ok(info)
}

pub(crate) fn write_local_screen_info_response(stream: &mut impl Write) -> io::Result<()> {
    let pixels = crate::cross_screen::current_physical_primary_screen_size()?;
    let millimeters = crate::cross_screen::current_primary_screen_millimeters()?;
    write_screen_info_response(
        stream,
        SynchronizedScreenInfo::new(pixels.width, pixels.height, millimeters.0, millimeters.1)?,
    )
}

fn write_screen_info_response(
    stream: &mut impl Write,
    info: SynchronizedScreenInfo,
) -> io::Result<()> {
    let mut response = [0_u8; RESPONSE_LENGTH];
    response[..8].copy_from_slice(SCREEN_INFO_ACK);
    for (index, value) in [
        info.pixel_width,
        info.pixel_height,
        info.physical_width_millimeters,
        info.physical_height_millimeters,
    ]
    .into_iter()
    .enumerate()
    {
        let start = 8 + index * 4;
        response[start..start + 4].copy_from_slice(&value.to_be_bytes());
    }
    stream.write_all(&response)?;
    stream.flush()
}

fn read_screen_info_response(stream: &mut impl Read) -> io::Result<SynchronizedScreenInfo> {
    let mut response = [0_u8; RESPONSE_LENGTH];
    stream.read_exact(&mut response)?;
    if &response[..8] != SCREEN_INFO_ACK {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "peer returned an invalid screen information response",
        ));
    }
    let read = |index: usize| {
        let start = 8 + index * 4;
        u32::from_be_bytes(
            response[start..start + 4]
                .try_into()
                .expect("fixed response field"),
        )
    };
    SynchronizedScreenInfo::new(read(0), read(1), read(2), read(3))
}

fn sleep_until_stopped(stop: &AtomicBool, duration: Duration) {
    let deadline = Instant::now()
        .checked_add(duration)
        .unwrap_or_else(Instant::now);
    while !stop.load(Ordering::Acquire) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;

    use crate::cross_screen::BoundaryScreen;
    use crate::security::TrustedTlsServer;

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let serial = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "pierdesk-screen-sync-{label}-{}-{serial}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn screen_information_frame_round_trips_and_rejects_invalid_dimensions() {
        let expected = SynchronizedScreenInfo::new(3_840, 2_160, 597, 336).unwrap();
        let mut encoded = Vec::new();
        write_screen_info_response(&mut encoded, expected).unwrap();
        assert_eq!(
            read_screen_info_response(&mut encoded.as_slice()).unwrap(),
            expected
        );

        encoded[8..12].copy_from_slice(&1_u32.to_be_bytes());
        let error = read_screen_info_response(&mut encoded.as_slice()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn pair_scoped_applied_update_does_not_replace_another_peers_pending_preview() {
        const LOCAL: &str = "00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:01";
        const FIRST: &str = "00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:02";
        const SECOND: &str = "00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:03";

        let screen = |fingerprint: &str, x| BoundaryScreen {
            device_fingerprint: fingerprint.to_owned(),
            pixel_width: 1_920,
            pixel_height: 1_080,
            physical_width_millimeters: 520,
            physical_height_millimeters: 290,
            rotation_quarter_turns: 0,
            canvas_x_millimeters: x,
            canvas_y_millimeters: 0,
        };
        let full = BoundaryLayoutV3 {
            schema_version: 3,
            revision: 8,
            local_fingerprint: LOCAL.to_owned(),
            screens: vec![screen(LOCAL, 0), screen(FIRST, 520), screen(SECOND, 1_040)],
            boundaries: Vec::new(),
        };
        let local = CertificateFingerprint::from_str(LOCAL).unwrap();
        let first = CertificateFingerprint::from_str(FIRST).unwrap();
        let second = CertificateFingerprint::from_str(SECOND).unwrap();
        let mut outbox = BoundaryOutbox::default();
        outbox.queue_for_layout(
            &full,
            BoundarySyncPhase::Preview,
            BoundarySyncSurface::Boundary,
            local,
        );
        let second_preview = outbox.latest_by_peer.get(&second).unwrap().clone();

        let mut first_applied = full.clone();
        first_applied
            .screens
            .retain(|value| value.device_fingerprint != SECOND);
        outbox.queue_for_layout(
            &first_applied,
            BoundarySyncPhase::Applied,
            BoundarySyncSurface::Complete,
            local,
        );

        let first_update = outbox.latest_by_peer.get(&first).unwrap();
        let second_update = outbox.latest_by_peer.get(&second).unwrap();
        assert_eq!(first_update.phase, BoundarySyncPhase::Applied);
        assert_eq!(first_update.surface, BoundarySyncSurface::Complete);
        assert!(first_update.generation > second_preview.generation);
        assert_eq!(second_update.phase, BoundarySyncPhase::Preview);
        assert_eq!(second_update.surface, BoundarySyncSurface::Boundary);
        assert_eq!(second_update.generation, second_preview.generation);
    }

    #[test]
    fn non_control_peer_fetches_screen_information_over_pinned_mutual_tls() {
        let client_directory = TestDirectory::new("client");
        let server_directory = TestDirectory::new("server");
        let client_store = DeviceStore::new(&client_directory.0);
        let server_store = DeviceStore::new(&server_directory.0);
        let client_identity = client_store.load_or_create_identity().unwrap().identity;
        let server_identity = server_store.load_or_create_identity().unwrap().identity;
        let client_fingerprint = client_identity.fingerprint();
        let server_fingerprint = server_identity.fingerprint();
        client_store
            .trust_peer(
                server_identity.certificate_der().to_vec(),
                server_fingerprint,
                "server",
            )
            .unwrap();
        server_store
            .trust_peer(
                client_identity.certificate_der().to_vec(),
                client_fingerprint,
                "client",
            )
            .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = listener.local_addr().unwrap();
        let expected = SynchronizedScreenInfo::new(2_560, 1_440, 344, 194).unwrap();
        let server = TrustedTlsServer::new(
            server_store.load_identity().unwrap(),
            server_store,
            Duration::from_secs(3),
        )
        .unwrap();
        let server_thread = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            let mut connection = server.accept(socket).unwrap();
            let mut magic = [0_u8; 8];
            connection.stream.read_exact(&mut magic).unwrap();
            assert_eq!(magic, *SCREEN_INFO_MAGIC);
            write_screen_info_response(&mut connection.stream, expected).unwrap();
        });

        let actual = query_peer_screen(&client_store, endpoint, server_fingerprint).unwrap();
        assert_eq!(actual, expected);
        server_thread.join().unwrap();
    }
}
