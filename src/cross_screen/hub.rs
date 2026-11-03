//! One listener and bounded authentication workers for all workspace peers.
//! This layer owns transport only; it never installs hooks or clipboard windows.
use std::collections::BTreeMap;
use std::io;
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::channel::{ChannelSnapshot, ChannelState, CrossScreenChannel, accept_workspace_socket};
use crate::app_settings::validate_private_endpoint;
use crate::device_store::DeviceStore;
use crate::security::CertificateFingerprint;
use crate::workspace::DeviceId;
use crate::workspace::persistence::ConnectionSettings;

const MAX_PENDING: usize = 8;
const AUTH_TIMEOUT: Duration = Duration::from_secs(1);
const AUTH_DEADLINE: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(20);
const RETRY: Duration = Duration::from_millis(200);
const MAX_RETRY: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub struct PeerLinkSnapshot {
    pub peer: DeviceId,
    pub endpoint: SocketAddr,
    pub dialer: bool,
    pub generation: u64,
    pub attempts: u64,
    pub last_error: Option<String>,
    pub channel: Option<ChannelSnapshot>,
}

struct PeerLink {
    endpoint: SocketAddr,
    channel: Option<Arc<CrossScreenChannel>>,
    generation: u64,
    attempts: u64,
    last_error: Option<String>,
    next_attempt: Instant,
    backoff: Duration,
}

struct Shared {
    local: DeviceId,
    stop: AtomicBool,
    peers: Mutex<BTreeMap<DeviceId, PeerLink>>,
    event_notifier: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

type SocketCancellation = Arc<Mutex<Option<TcpStream>>>;

struct Pending {
    peer: Option<DeviceId>,
    started: Instant,
    socket: SocketCancellation,
    worker: JoinHandle<io::Result<(DeviceId, CrossScreenChannel)>>,
}

impl Pending {
    fn cancel(&self) {
        if let Some(socket) = self
            .socket
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }
}

pub struct PeerConnectionHub {
    store: DeviceStore,
    shared: Arc<Shared>,
    worker: Mutex<Option<JoinHandle<()>>>,
    listen_address: SocketAddr,
}

impl PeerConnectionHub {
    /// Start a single fixed-port listener from persisted per-peer settings.
    ///
    /// # Errors
    /// Returns configuration, trust, bind or worker-start failures.
    pub fn start(store: DeviceStore, settings: &ConnectionSettings) -> io::Result<Self> {
        settings.validate(&store)?;
        let address = SocketAddr::new(settings.local_endpoint.ip(), super::AUTO_CHANNEL_PORT);
        let peers = settings
            .peers
            .iter()
            .filter(|p| p.enabled)
            .map(|p| {
                let address = p.manual_endpoint.unwrap_or(p.last_endpoint);
                (
                    p.peer,
                    SocketAddr::new(address.ip(), super::AUTO_CHANNEL_PORT),
                )
            })
            .collect();
        Self::from_listener(store, TcpListener::bind(address)?, peers)
    }

    /// Own an already bound listener, also used by isolated multi-device diagnostics.
    ///
    /// # Errors
    /// Rejects unsafe addresses, more than four peers, missing trust or startup failure.
    pub fn from_listener(
        store: DeviceStore,
        listener: TcpListener,
        peers: BTreeMap<DeviceId, SocketAddr>,
    ) -> io::Result<Self> {
        let listen_address = listener.local_addr()?;
        validate_private_endpoint(listen_address)?;
        let local = store.load_identity()?.fingerprint().bytes();
        if peers.len() > 4 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "workspace supports at most four remote peers",
            ));
        }
        for (peer, address) in &peers {
            validate_private_endpoint(*address)?;
            if *peer == local || address.ip().is_loopback() != listen_address.ip().is_loopback() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid workspace peer or endpoint scope",
                ));
            }
            store.load_peer(CertificateFingerprint::from_bytes(*peer))?;
        }
        listener.set_nonblocking(true)?;
        let shared = Arc::new(Shared {
            local,
            stop: AtomicBool::new(false),
            peers: Mutex::new(
                peers
                    .into_iter()
                    .map(|(id, endpoint)| {
                        (
                            id,
                            PeerLink {
                                endpoint,
                                channel: None,
                                generation: 0,
                                attempts: 0,
                                last_error: None,
                                next_attempt: Instant::now(),
                                backoff: RETRY,
                            },
                        )
                    })
                    .collect(),
            ),
            event_notifier: Mutex::new(None),
        });
        let background = Arc::clone(&shared);
        let worker_store = store.clone();
        let worker = std::thread::Builder::new()
            .name("pierdesk-peer-hub".into())
            .spawn(move || run(&worker_store, &listener, &background))?;
        Ok(Self {
            store,
            shared,
            worker: Mutex::new(Some(worker)),
            listen_address,
        })
    }

    /// Add newly discovered workspace peers or refresh their addresses without restarting the
    /// listener or native input owner. Existing authenticated links stay live when their endpoint
    /// is unchanged.
    ///
    /// # Errors
    /// Rejects invalid, untrusted, local, out-of-scope, or excess peer endpoints.
    pub fn upsert_peers(&self, peers: BTreeMap<DeviceId, SocketAddr>) -> io::Result<()> {
        if peers.len() > 4 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "workspace supports at most four remote peers",
            ));
        }
        for (peer, address) in &peers {
            validate_private_endpoint(*address)?;
            if *peer == self.shared.local
                || address.ip().is_loopback() != self.listen_address.ip().is_loopback()
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid workspace peer or endpoint scope",
                ));
            }
            self.store
                .load_peer(CertificateFingerprint::from_bytes(*peer))?;
        }
        let mut links = self
            .shared
            .peers
            .lock()
            .map_err(|_| io::Error::other("workspace peer map poisoned"))?;
        let distinct = links
            .keys()
            .copied()
            .chain(peers.keys().copied())
            .collect::<std::collections::BTreeSet<_>>();
        if distinct.len() > 4 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "workspace supports at most four remote peers",
            ));
        }
        for (peer, endpoint) in peers {
            match links.get_mut(&peer) {
                Some(link) if link.endpoint == endpoint => {}
                Some(link) => {
                    if let Some(channel) = link.channel.take() {
                        channel.request_close();
                    }
                    link.endpoint = endpoint;
                    link.last_error = None;
                    link.next_attempt = Instant::now();
                    link.backoff = RETRY;
                }
                None => {
                    links.insert(
                        peer,
                        PeerLink {
                            endpoint,
                            channel: None,
                            generation: 0,
                            attempts: 0,
                            last_error: None,
                            next_attempt: Instant::now(),
                            backoff: RETRY,
                        },
                    );
                }
            }
        }
        Ok(())
    }

    #[must_use]
    pub const fn listen_address(&self) -> SocketAddr {
        self.listen_address
    }

    /// Settings-facing aggregate; latency and errors retain their peer identity.
    #[must_use]
    pub fn settings_model(&self) -> serde_json::Value {
        let peers: Vec<_> = self
            .snapshots()
            .into_iter()
            .map(|link| {
                let online = link
                    .channel
                    .as_ref()
                    .is_some_and(|c| c.state == ChannelState::Connected);
                serde_json::json!({
                    "fingerprint": CertificateFingerprint::from_bytes(link.peer).to_string(),
                    "endpoint": link.endpoint.to_string(), "online": online,
                    "generation": link.generation, "attempts": link.attempts,
                    "roundTripMicros": link.channel.as_ref().and_then(|c| c.round_trip_micros),
                    "lastError": link.last_error,
                })
            })
            .collect();
        serde_json::json!({"schema": 1, "onlineCount": peers.iter().filter(|p| p["online"] == true).count(), "peerCount": peers.len(), "peers": peers})
    }

    #[must_use]
    pub fn snapshots(&self) -> Vec<PeerLinkSnapshot> {
        self.shared
            .peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(peer, link)| PeerLinkSnapshot {
                peer: *peer,
                endpoint: link.endpoint,
                dialer: self.shared.local < *peer,
                generation: link.generation,
                attempts: link.attempts,
                last_error: link.last_error.clone(),
                channel: link.channel.as_ref().map(|c| c.snapshot()),
            })
            .collect()
    }

    /// Dispatch to one authenticated link. The peer map lock is released before dispatch.
    ///
    /// # Errors
    /// Returns `NotConnected` for an offline peer, otherwise propagates the handler error.
    pub fn with_peer<T>(
        &self,
        peer: DeviceId,
        dispatch: impl FnOnce(&CrossScreenChannel) -> io::Result<T>,
    ) -> io::Result<T> {
        let channel = self
            .shared
            .peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&peer)
            .and_then(|p| p.channel.clone())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotConnected, "workspace peer is offline")
            })?;
        dispatch(&channel)
    }

    /// Wake the product input pump whenever any authenticated peer queues an event.
    pub(crate) fn set_event_notifier(&self, notifier: Arc<dyn Fn() + Send + Sync>) {
        let channel_notifier = Arc::clone(&notifier);
        *self
            .shared
            .event_notifier
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(notifier);
        let channels = self
            .shared
            .peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter_map(|link| link.channel.clone())
            .collect::<Vec<_>>();
        for channel in channels {
            channel.set_event_notifier(Arc::clone(&channel_notifier));
        }
    }

    pub fn stop(&mut self) {
        self.shutdown();
    }

    /// Stop and join the shared listener even while services retain hub references.
    pub fn shutdown(&self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(worker) = self
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = worker.join();
        }
    }
}

impl Drop for PeerConnectionHub {
    fn drop(&mut self) {
        self.stop();
    }
}

fn spawn_incoming(
    store: &DeviceStore,
    shared: &Arc<Shared>,
    socket: TcpStream,
) -> io::Result<Pending> {
    let cancel = Arc::new(Mutex::new(Some(socket.try_clone()?)));
    let allowed = shared
        .peers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .keys()
        .map(|p| CertificateFingerprint::from_bytes(*p))
        .collect::<Vec<_>>();
    let store = store.clone();
    let shared = Arc::clone(shared);
    let worker = std::thread::Builder::new()
        .name("pierdesk-peer-auth".into())
        .spawn(move || {
            accept_workspace_socket(&store, socket, &allowed, AUTH_TIMEOUT, &shared.stop)
                .map(|(peer, channel)| (peer.bytes(), channel))
        })?;
    Ok(Pending {
        peer: None,
        started: Instant::now(),
        socket: cancel,
        worker,
    })
}

fn spawn_outgoing(store: &DeviceStore, peer: DeviceId, address: SocketAddr) -> io::Result<Pending> {
    let cancel = Arc::new(Mutex::new(None));
    let worker_cancel = Arc::clone(&cancel);
    let store = store.clone();
    let worker = std::thread::Builder::new()
        .name("pierdesk-peer-dial".into())
        .spawn(move || {
            let socket = TcpStream::connect_timeout(&address, AUTH_TIMEOUT)?;
            *worker_cancel
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(socket.try_clone()?);
            CrossScreenChannel::connect_socket(
                &store,
                CertificateFingerprint::from_bytes(peer),
                socket,
                AUTH_TIMEOUT,
            )
            .map(|channel| (peer, channel))
        })?;
    Ok(Pending {
        peer: Some(peer),
        started: Instant::now(),
        socket: cancel,
        worker,
    })
}

fn finish_pending(shared: &Shared, pending: Pending) -> Option<Arc<CrossScreenChannel>> {
    let result = pending
        .worker
        .join()
        .unwrap_or_else(|_| Err(io::Error::other("peer authentication worker panicked")));
    let event_notifier = shared
        .event_notifier
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let mut peers = shared
        .peers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match result {
        Ok((peer, channel)) => {
            if let Some(link) = peers.get_mut(&peer)
                && link.channel.is_none()
            {
                let channel = Arc::new(channel);
                if let Some(notifier) = event_notifier {
                    channel.set_event_notifier(notifier);
                }
                link.channel = Some(channel);
                link.generation = link.generation.saturating_add(1);
                link.last_error = None;
                link.backoff = RETRY;
            } else {
                // Keep the committed connection; a duplicate must not replace its epoch.
                drop(peers);
                channel.request_close();
                return Some(Arc::new(channel));
            }
        }
        Err(error) => {
            if let Some(peer) = pending.peer
                && let Some(link) = peers.get_mut(&peer)
            {
                link.last_error = Some(error.to_string());
                link.next_attempt = Instant::now() + link.backoff;
                link.backoff = (link.backoff * 2).min(MAX_RETRY);
            }
        }
    }
    None
}

fn run(store: &DeviceStore, listener: &TcpListener, shared: &Arc<Shared>) {
    let mut pending: Vec<Pending> = Vec::new();
    let mut retiring: Vec<Arc<CrossScreenChannel>> = Vec::new();
    let mut last_trust_check = Instant::now();
    while !shared.stop.load(Ordering::Acquire) {
        retiring.retain(|channel| !channel.workers_finished());
        let mut index = 0;
        while index < pending.len() {
            if pending[index].worker.is_finished() {
                if let Some(channel) = finish_pending(shared, pending.swap_remove(index)) {
                    retiring.push(channel);
                }
            } else {
                if pending[index].started.elapsed() >= AUTH_DEADLINE {
                    pending[index].cancel();
                }
                index += 1;
            }
        }
        let check_trust = last_trust_check.elapsed() >= Duration::from_secs(1);
        if check_trust {
            last_trust_check = Instant::now();
        }
        let mut expired = Vec::new();
        let mut dial = Vec::new();
        {
            let mut peers = shared
                .peers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (peer, link) in &mut *peers {
                let revoked = check_trust
                    && store
                        .load_peer(CertificateFingerprint::from_bytes(*peer))
                        .is_err();
                if let Some(channel) = &link.channel {
                    let snapshot = channel.snapshot();
                    if revoked || snapshot.state != ChannelState::Connected {
                        link.last_error = Some(if revoked {
                            "peer trust revoked".into()
                        } else {
                            snapshot
                                .last_error
                                .unwrap_or_else(|| "peer disconnected".into())
                        });
                        if let Some(channel) = link.channel.take() {
                            channel.request_close();
                            expired.push(channel);
                        }
                        link.next_attempt = Instant::now() + RETRY;
                    }
                }
                if !revoked
                    && shared.local < *peer
                    && link.channel.is_none()
                    && Instant::now() >= link.next_attempt
                    && !pending.iter().any(|p| p.peer == Some(*peer))
                    && pending.len() + retiring.len() + dial.len() < MAX_PENDING
                {
                    link.attempts = link.attempts.saturating_add(1);
                    link.next_attempt = Instant::now() + MAX_RETRY;
                    dial.push((*peer, link.endpoint));
                }
            }
        }
        for (peer, endpoint) in dial {
            match spawn_outgoing(store, peer, endpoint) {
                Ok(job) => pending.push(job),
                Err(error) => {
                    if let Some(link) = shared
                        .peers
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .get_mut(&peer)
                    {
                        link.last_error = Some(error.to_string());
                    }
                }
            }
        }
        // Limit accepts per tick so a connection flood cannot starve peer maintenance.
        for _ in 0..MAX_PENDING {
            match listener.accept() {
                Ok((socket, _)) if pending.len() + retiring.len() < MAX_PENDING => {
                    if let Ok(job) = spawn_incoming(store, shared, socket) {
                        pending.push(job);
                    }
                }
                Ok((socket, _)) => {
                    let _ = socket.shutdown(Shutdown::Both);
                }
                Err(_) => break,
            }
        }
        retiring.extend(expired);
        std::thread::sleep(POLL);
    }
    shutdown(shared, pending, retiring);
}

fn shutdown(shared: &Shared, pending: Vec<Pending>, mut channels: Vec<Arc<CrossScreenChannel>>) {
    channels.extend({
        let mut peers = shared
            .peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        peers
            .values_mut()
            .filter_map(|p| p.channel.take())
            .collect::<Vec<_>>()
    });
    for channel in &channels {
        channel.request_close();
    }
    for job in &pending {
        job.cancel();
    }
    for job in pending {
        // Outgoing connect may finish after the first cancellation attempt.
        while !job.worker.is_finished() {
            job.cancel();
            std::thread::sleep(POLL);
        }
        if let Ok(Ok((_, channel))) = job.worker.join() {
            channel.request_close();
        }
    }
    drop(channels);
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::{ChannelControlEvent, PhysicalScreenSize};
    use super::*;

    pub(crate) struct Network {
        pub(crate) root: std::path::PathBuf,
        pub(crate) stores: Vec<DeviceStore>,
        pub(crate) ids: Vec<DeviceId>,
        pub(crate) hubs: Vec<PeerConnectionHub>,
    }

    impl Network {
        pub(crate) fn new(count: usize) -> Self {
            Self::with_links(count, |a, b| a != b)
        }

        pub(crate) fn with_links(count: usize, connected: impl Fn(usize, usize) -> bool) -> Self {
            let mut random = [0; 8];
            getrandom::fill(&mut random).unwrap();
            let root = std::env::temp_dir().join(format!(
                "pierdesk-hub-test-{:016x}",
                u64::from_le_bytes(random)
            ));
            let stores: Vec<_> = (0..count)
                .map(|i| DeviceStore::new(root.join(i.to_string())))
                .collect();
            let identities: Vec<_> = stores
                .iter()
                .map(|s| s.load_or_create_identity().unwrap().identity)
                .collect();
            let ids: Vec<_> = identities.iter().map(|i| i.fingerprint().bytes()).collect();
            for (index, store) in stores.iter().enumerate() {
                for (other, identity) in identities.iter().enumerate() {
                    assert_eq!(connected(index, other), connected(other, index));
                    if index != other && connected(index, other) {
                        store
                            .trust_peer(
                                identity.certificate_der().to_vec(),
                                identity.fingerprint(),
                                &format!("peer-{other}"),
                            )
                            .unwrap();
                    }
                }
            }
            let listeners: Vec<_> = (0..count)
                .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
                .collect();
            let addresses: Vec<_> = listeners.iter().map(|l| l.local_addr().unwrap()).collect();
            let hubs = listeners
                .into_iter()
                .enumerate()
                .map(|(index, listener)| {
                    let peers = ids
                        .iter()
                        .zip(&addresses)
                        .enumerate()
                        .filter(|(other, _)| index != *other && connected(index, *other))
                        .map(|(_, (id, addr))| (*id, *addr))
                        .collect();
                    PeerConnectionHub::from_listener(stores[index].clone(), listener, peers)
                        .unwrap()
                })
                .collect();
            Self {
                root,
                stores,
                ids,
                hubs,
            }
        }

        pub(crate) fn wait_connected(&self) {
            wait_until(|| {
                self.hubs.iter().all(|hub| {
                    hub.snapshots().iter().all(|p| {
                        p.channel.as_ref().is_some_and(|c| {
                            c.state == ChannelState::Connected && c.round_trip_micros.is_some()
                        })
                    })
                })
            });
        }
    }

    impl Drop for Network {
        fn drop(&mut self) {
            // Stop all supervisors together so close acknowledgements can run concurrently.
            for hub in &self.hubs {
                hub.shared.stop.store(true, Ordering::Release);
            }
            self.hubs.clear();
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn wait_until(mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !predicate() {
            assert!(Instant::now() < deadline, "multi-peer condition timed out");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn five_devices_form_ten_mutual_tls_links_and_dispatch_to_the_exact_peer() {
        let network = Network::new(5);
        network.wait_connected();
        let model = network.hubs[0].settings_model();
        assert_eq!(model["onlineCount"], 4);
        assert_eq!(model["peerCount"], 4);
        assert_eq!(
            network
                .hubs
                .iter()
                .map(|h| h.snapshots().len())
                .sum::<usize>(),
            20
        );
        for i in 1..5 {
            network.hubs[0]
                .with_peer(network.ids[i], |c| {
                    c.send_screen_info(PhysicalScreenSize {
                        width: 1000 + u32::try_from(i).unwrap(),
                        height: 800,
                    })
                })
                .unwrap();
        }
        for i in 1..5 {
            let mut event = None;
            wait_until(|| {
                event = network.hubs[i]
                    .with_peer(
                        network.ids[0],
                        CrossScreenChannel::try_receive_control_event,
                    )
                    .unwrap();
                event.is_some()
            });
            assert_eq!(
                event,
                Some(ChannelControlEvent::ScreenInfo(PhysicalScreenSize {
                    width: 1000 + u32::try_from(i).unwrap(),
                    height: 800
                }))
            );
        }
        for (i, hub) in network.hubs.iter().enumerate() {
            for peer in hub.snapshots() {
                let other = network.ids.iter().position(|id| *id == peer.peer).unwrap();
                let reverse = network.hubs[other]
                    .snapshots()
                    .into_iter()
                    .find(|p| p.peer == network.ids[i])
                    .unwrap();
                let a = peer.channel.unwrap();
                let b = reverse.channel.unwrap();
                assert_eq!(a.session_id, b.session_id);
                assert_eq!(a.channel_epoch, b.channel_epoch);
                assert!(peer.generation >= 1);
            }
        }
    }

    #[test]
    fn one_peer_shutdown_does_not_reset_other_channels() {
        let mut network = Network::new(3);
        network.wait_connected();
        let stable = network.hubs[0]
            .snapshots()
            .into_iter()
            .find(|p| p.peer == network.ids[1])
            .unwrap();
        let epoch = stable.channel.unwrap().channel_epoch;
        network.hubs[2].stop();
        wait_until(|| {
            network.hubs[0]
                .snapshots()
                .iter()
                .any(|p| p.peer == network.ids[2] && p.channel.is_none())
        });
        let current = network.hubs[0]
            .snapshots()
            .into_iter()
            .find(|p| p.peer == network.ids[1])
            .unwrap();
        assert_eq!(current.generation, stable.generation);
        assert_eq!(current.channel.unwrap().channel_epoch, epoch);
        network.hubs[0]
            .with_peer(network.ids[1], |c| {
                c.send_screen_info(PhysicalScreenSize {
                    width: 1920,
                    height: 1080,
                })
            })
            .unwrap();
        wait_until(|| {
            network.hubs[1]
                .with_peer(
                    network.ids[0],
                    CrossScreenChannel::try_receive_control_event,
                )
                .unwrap()
                .is_some()
        });
    }

    #[test]
    fn stalled_tls_connections_do_not_block_heartbeats_or_bounded_shutdown() {
        let mut network = Network::new(3);
        network.wait_connected();
        let stalled: Vec<_> = (0..MAX_PENDING)
            .map(|_| TcpStream::connect(network.hubs[0].listen_address()).unwrap())
            .collect();
        let before = network.hubs[0]
            .snapshots()
            .iter()
            .map(|p| p.channel.as_ref().unwrap().received_heartbeats)
            .sum::<u64>();
        wait_until(|| {
            network.hubs[0]
                .snapshots()
                .iter()
                .map(|p| p.channel.as_ref().unwrap().received_heartbeats)
                .sum::<u64>()
                > before
        });
        let started = Instant::now();
        network.hubs[0].stop();
        assert!(started.elapsed() < Duration::from_secs(5));
        drop(stalled);
    }

    #[test]
    fn revoked_peer_disconnects_without_affecting_other_trusted_peers() {
        let network = Network::new(3);
        network.wait_connected();
        network.stores[0]
            .revoke_peer(CertificateFingerprint::from_bytes(network.ids[2]))
            .unwrap();
        wait_until(|| {
            network.hubs[0]
                .snapshots()
                .iter()
                .any(|p| p.peer == network.ids[2] && p.channel.is_none())
        });
        assert!(network.hubs[0].snapshots().iter().any(|p| {
            p.peer == network.ids[1]
                && p.channel
                    .as_ref()
                    .is_some_and(|c| c.state == ChannelState::Connected)
        }));
    }

    #[test]
    fn reconnect_changes_epoch_without_replacing_other_peer_sessions() {
        let mut network = Network::new(3);
        network.wait_connected();
        let old = network.hubs[0]
            .snapshots()
            .into_iter()
            .find(|p| p.peer == network.ids[2])
            .unwrap();
        let old_epoch = old.channel.unwrap().channel_epoch;
        let address = network.hubs[2].listen_address();
        let endpoints = network.hubs[2]
            .snapshots()
            .into_iter()
            .map(|p| (p.peer, p.endpoint))
            .collect();
        network.hubs[2].stop();
        wait_until(|| {
            network.hubs[0]
                .snapshots()
                .iter()
                .any(|p| p.peer == network.ids[2] && p.channel.is_none())
        });
        network.hubs[2] = PeerConnectionHub::from_listener(
            network.stores[2].clone(),
            TcpListener::bind(address).unwrap(),
            endpoints,
        )
        .unwrap();
        network.wait_connected();
        let current = network.hubs[0]
            .snapshots()
            .into_iter()
            .find(|p| p.peer == network.ids[2])
            .unwrap();
        assert!(current.generation > old.generation);
        assert_ne!(current.channel.unwrap().channel_epoch, old_epoch);
    }

    #[test]
    fn duplicate_authenticated_connection_keeps_the_committed_epoch() {
        let network = Network::new(2);
        network.wait_connected();
        let (dialer, acceptor) = if network.ids[0] < network.ids[1] {
            (0, 1)
        } else {
            (1, 0)
        };
        let before = network.hubs[acceptor].snapshots()[0]
            .channel
            .as_ref()
            .unwrap()
            .channel_epoch;
        let duplicate = CrossScreenChannel::connect(
            &network.stores[dialer],
            CertificateFingerprint::from_bytes(network.ids[acceptor]),
            network.hubs[acceptor].listen_address(),
            AUTH_TIMEOUT,
        )
        .unwrap();
        wait_until(|| duplicate.snapshot().state != ChannelState::Connected);
        assert_eq!(
            network.hubs[acceptor].snapshots()[0]
                .channel
                .as_ref()
                .unwrap()
                .channel_epoch,
            before
        );
        assert_eq!(network.hubs[acceptor].snapshots()[0].generation, 1);
    }

    #[test]
    fn trusted_device_outside_workspace_is_rejected_before_channel_hello() {
        let mut network = Network::new(2);
        network.wait_connected();
        let (dialer, acceptor) = if network.ids[0] < network.ids[1] {
            (0, 1)
        } else {
            (1, 0)
        };
        network.hubs[dialer].stop();
        network.hubs[acceptor].stop();
        network.hubs[acceptor] = PeerConnectionHub::from_listener(
            network.stores[acceptor].clone(),
            TcpListener::bind("127.0.0.1:0").unwrap(),
            BTreeMap::new(),
        )
        .unwrap();
        assert!(
            CrossScreenChannel::connect(
                &network.stores[dialer],
                CertificateFingerprint::from_bytes(network.ids[acceptor]),
                network.hubs[acceptor].listen_address(),
                AUTH_TIMEOUT
            )
            .is_err()
        );
        assert!(network.hubs[acceptor].snapshots().is_empty());
    }

    #[test]
    fn discovered_peers_join_without_restarting_the_listener() {
        let mut network = Network::new(2);
        network.wait_connected();
        for hub in &mut network.hubs {
            hub.stop();
        }
        let listeners: Vec<_> = (0..2)
            .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
            .collect();
        let addresses: Vec<_> = listeners
            .iter()
            .map(|listener| listener.local_addr().unwrap())
            .collect();
        network.hubs = listeners
            .into_iter()
            .enumerate()
            .map(|(index, listener)| {
                PeerConnectionHub::from_listener(
                    network.stores[index].clone(),
                    listener,
                    BTreeMap::new(),
                )
                .unwrap()
            })
            .collect();

        assert!(network.hubs.iter().all(|hub| hub.snapshots().is_empty()));
        network.hubs[0]
            .upsert_peers(BTreeMap::from([(network.ids[1], addresses[1])]))
            .unwrap();
        network.hubs[1]
            .upsert_peers(BTreeMap::from([(network.ids[0], addresses[0])]))
            .unwrap();
        network.wait_connected();

        assert_eq!(network.hubs[0].listen_address(), addresses[0]);
        assert_eq!(network.hubs[1].listen_address(), addresses[1]);
        assert_eq!(network.hubs[0].snapshots()[0].peer, network.ids[1]);
        assert_eq!(network.hubs[1].snapshots()[0].peer, network.ids[0]);
    }

    #[test]
    fn full_snapshots_and_other_protocols_are_demultiplexed_over_the_same_tls_link() {
        use super::super::ApplicationProtocol;
        let network = Network::new(3);
        network.wait_connected();
        let protocols = [
            ApplicationProtocol::Layout,
            ApplicationProtocol::Clipboard,
            ApplicationProtocol::File,
            ApplicationProtocol::Control,
        ];
        for protocol in protocols {
            network.hubs[0]
                .with_peer(network.ids[1], |c| {
                    c.send_application(protocol, vec![protocol as u8 + 1; 12000])
                })
                .unwrap();
        }
        for protocol in protocols {
            let mut received = None;
            wait_until(|| {
                received = network.hubs[1]
                    .with_peer(network.ids[0], |c| c.try_receive_application(protocol))
                    .unwrap();
                received.is_some()
            });
            assert_eq!(received.unwrap(), vec![protocol as u8 + 1; 12000]);
            assert!(
                network.hubs[2]
                    .with_peer(network.ids[0], |c| c.try_receive_application(protocol))
                    .unwrap()
                    .is_none()
            );
        }
    }
}
