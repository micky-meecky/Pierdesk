use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, NO_ERROR};
use windows::Win32::NetworkManagement::IpHelper::{
    GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, GetAdaptersAddresses,
    IP_ADAPTER_ADDRESSES_LH,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};

use crate::app_settings::AppSettings;
use crate::device_store::DeviceStore;
use crate::security::CertificateFingerprint;

pub const DISCOVERY_PORT: u16 = 45_231;
pub const PAIRING_PORT: u16 = 45_232;
pub const SERVICE_PORT: u16 = 45_233;

const MAGIC: &[u8; 8] = b"CFDISC01";
const VERSION: u16 = 1;
const ANNOUNCEMENT: u8 = 1;
const RESPONSE: u8 = 2;
const HEADER_LENGTH: usize = 8 + 2 + 1 + 32 + 2 + 2 + 1;
const LABEL_LIMIT: usize = 63;
const PACKET_LIMIT: usize = HEADER_LENGTH + LABEL_LIMIT;
const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(2);
const RECEIVE_TIMEOUT: Duration = Duration::from_millis(400);
const PEER_TTL: Duration = Duration::from_secs(12);
const NETWORK_ISSUE_GRACE: Duration = Duration::from_millis(1_500);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredPeer {
    pub fingerprint: CertificateFingerprint,
    pub label: String,
    pub service_endpoint: SocketAddr,
    pub pairing_endpoint: SocketAddr,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Packet {
    kind: u8,
    fingerprint: CertificateFingerprint,
    service_port: u16,
    pairing_port: u16,
    label: String,
}

impl Packet {
    fn new(kind: u8, fingerprint: CertificateFingerprint, label: &str) -> Self {
        Self {
            kind,
            fingerprint,
            service_port: SERVICE_PORT,
            pairing_port: PAIRING_PORT,
            label: bounded_label(label),
        }
    }

    fn encode(&self) -> io::Result<Vec<u8>> {
        if !matches!(self.kind, ANNOUNCEMENT | RESPONSE)
            || self.service_port != SERVICE_PORT
            || self.pairing_port != PAIRING_PORT
        {
            return Err(invalid_data("invalid discovery packet fields"));
        }
        let label = self.label.as_bytes();
        if label.is_empty() || label.len() > LABEL_LIMIT {
            return Err(invalid_data("invalid discovery device label"));
        }
        let label_length = u8::try_from(label.len())
            .map_err(|_| invalid_data("discovery device label is too long"))?;
        let mut encoded = Vec::with_capacity(HEADER_LENGTH + label.len());
        encoded.extend_from_slice(MAGIC);
        encoded.extend_from_slice(&VERSION.to_le_bytes());
        encoded.push(self.kind);
        encoded.extend_from_slice(&self.fingerprint.bytes());
        encoded.extend_from_slice(&self.service_port.to_le_bytes());
        encoded.extend_from_slice(&self.pairing_port.to_le_bytes());
        encoded.push(label_length);
        encoded.extend_from_slice(label);
        Ok(encoded)
    }

    fn decode(encoded: &[u8]) -> io::Result<Self> {
        if encoded.len() < HEADER_LENGTH || encoded.len() > PACKET_LIMIT {
            return Err(invalid_data("invalid discovery packet length"));
        }
        if &encoded[..8] != MAGIC {
            return Err(invalid_data("discovery packet magic mismatch"));
        }
        if u16::from_le_bytes([encoded[8], encoded[9]]) != VERSION {
            return Err(invalid_data("unsupported discovery packet version"));
        }
        let kind = encoded[10];
        if !matches!(kind, ANNOUNCEMENT | RESPONSE) {
            return Err(invalid_data("unknown discovery packet kind"));
        }
        let mut fingerprint = [0_u8; 32];
        fingerprint.copy_from_slice(&encoded[11..43]);
        let service_port = u16::from_le_bytes([encoded[43], encoded[44]]);
        let pairing_port = u16::from_le_bytes([encoded[45], encoded[46]]);
        if service_port != SERVICE_PORT || pairing_port != PAIRING_PORT {
            return Err(invalid_data(
                "discovery packet contains unsupported service ports",
            ));
        }
        let label_length = usize::from(encoded[47]);
        if label_length == 0
            || label_length > LABEL_LIMIT
            || encoded.len() != HEADER_LENGTH + label_length
        {
            return Err(invalid_data("invalid discovery device label length"));
        }
        let label = std::str::from_utf8(&encoded[HEADER_LENGTH..])
            .map_err(|_| invalid_data("discovery device label is not UTF-8"))?;
        if label.chars().any(char::is_control) {
            return Err(invalid_data(
                "discovery device label contains control characters",
            ));
        }
        Ok(Self {
            kind,
            fingerprint: CertificateFingerprint::from_bytes(fingerprint),
            service_port,
            pairing_port,
            label: label.to_owned(),
        })
    }
}

#[derive(Clone)]
struct SeenPeer {
    peer: DiscoveredPeer,
    last_seen: Instant,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NetworkPathIssue {
    pub fingerprint: CertificateFingerprint,
    pub peer_label: String,
    pub peer_endpoint: SocketAddr,
    pub local_adapter: String,
}

impl NetworkPathIssue {
    #[must_use]
    pub fn user_message(&self) -> String {
        format!(
            "对端地址 {} 同时属于本机网卡“{}”。Pierdesk 已保留原配置并暂停使用这条冲突路径；请关闭冲突的本机热点或虚拟网卡，或让两台电脑连接同一局域网。冲突解除后会自动恢复。",
            self.peer_endpoint.ip(),
            self.local_adapter
        )
    }

    #[must_use]
    pub fn log_fields(&self) -> String {
        format!(
            "state=address_conflict peer={} peer_fingerprint={} local_adapter={:?} action=preserved_settings",
            self.peer_endpoint.ip(),
            self.fingerprint,
            self.local_adapter
        )
    }
}

#[derive(Clone)]
struct SeenNetworkIssue {
    issue: NetworkPathIssue,
    first_seen: Instant,
    last_seen: Instant,
    active: bool,
}

#[derive(Clone)]
pub struct DiscoveryView {
    peers: Arc<Mutex<HashMap<CertificateFingerprint, SeenPeer>>>,
    network_issues: Arc<Mutex<HashMap<CertificateFingerprint, SeenNetworkIssue>>>,
}

impl DiscoveryView {
    #[must_use]
    pub fn peers(&self) -> Vec<DiscoveredPeer> {
        let now = Instant::now();
        let Ok(mut peers) = self.peers.lock() else {
            return Vec::new();
        };
        peers.retain(|_, peer| now.saturating_duration_since(peer.last_seen) <= PEER_TTL);
        let mut result = peers
            .values()
            .map(|peer| peer.peer.clone())
            .collect::<Vec<_>>();
        result.sort_by(|left, right| {
            left.label.cmp(&right.label).then_with(|| {
                left.fingerprint
                    .to_string()
                    .cmp(&right.fingerprint.to_string())
            })
        });
        result
    }

    #[must_use]
    pub fn peer(&self, fingerprint: CertificateFingerprint) -> Option<DiscoveredPeer> {
        self.peers()
            .into_iter()
            .find(|peer| peer.fingerprint == fingerprint)
    }

    #[must_use]
    pub fn network_issue(&self, fingerprint: CertificateFingerprint) -> Option<NetworkPathIssue> {
        if self.peer(fingerprint).is_some() {
            return None;
        }
        let now = Instant::now();
        let Ok(mut issues) = self.network_issues.lock() else {
            return None;
        };
        issues.retain(|_, issue| now.saturating_duration_since(issue.last_seen) <= PEER_TTL);
        issues
            .get(&fingerprint)
            .filter(|issue| issue.active)
            .map(|issue| issue.issue.clone())
    }
}

pub struct DiscoveryRuntime {
    view: DiscoveryView,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl DiscoveryRuntime {
    /// Starts the bounded local-LAN discovery worker.
    ///
    /// Discovery packets contain only a public certificate fingerprint, a bounded device label,
    /// and service ports. They are address hints, never trust assertions.
    ///
    /// # Errors
    ///
    /// Returns an error when identity loading, UDP binding, socket configuration, or worker
    /// creation fails.
    pub fn start(store: DeviceStore) -> io::Result<Self> {
        let identity = store.load_or_create_identity()?.identity;
        let fingerprint = identity.fingerprint();
        let label = local_device_label();
        let socket = UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, DISCOVERY_PORT)))?;
        socket.set_broadcast(true)?;
        socket.set_read_timeout(Some(RECEIVE_TIMEOUT))?;
        let peers = Arc::new(Mutex::new(HashMap::new()));
        let network_issues = Arc::new(Mutex::new(HashMap::new()));
        let view = DiscoveryView {
            peers: Arc::clone(&peers),
            network_issues: Arc::clone(&network_issues),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::Builder::new()
            .name("pierdesk-discovery".to_owned())
            .spawn(move || {
                discovery_loop(
                    &socket,
                    &store,
                    fingerprint,
                    &label,
                    &peers,
                    &network_issues,
                    &worker_stop,
                );
            })?;
        Ok(Self {
            view,
            stop,
            worker: Some(worker),
        })
    }

    #[must_use]
    pub fn view(&self) -> DiscoveryView {
        self.view.clone()
    }
}

impl Drop for DiscoveryRuntime {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = UdpSocket::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).and_then(|socket| {
            socket.send_to(
                &[0],
                SocketAddr::from((Ipv4Addr::LOCALHOST, DISCOVERY_PORT)),
            )?;
            Ok(())
        });
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn discovery_loop(
    socket: &UdpSocket,
    store: &DeviceStore,
    fingerprint: CertificateFingerprint,
    label: &str,
    peers: &Arc<Mutex<HashMap<CertificateFingerprint, SeenPeer>>>,
    network_issues: &Arc<Mutex<HashMap<CertificateFingerprint, SeenNetworkIssue>>>,
    stop: &AtomicBool,
) {
    let announcement = Packet::new(ANNOUNCEMENT, fingerprint, label)
        .encode()
        .expect("local discovery packet must be valid");
    let response = Packet::new(RESPONSE, fingerprint, label)
        .encode()
        .expect("local discovery packet must be valid");
    let mut last_announcement = Instant::now()
        .checked_sub(ANNOUNCE_INTERVAL)
        .unwrap_or_else(Instant::now);
    let mut buffer = [0_u8; PACKET_LIMIT + 1];
    while !stop.load(Ordering::Acquire) {
        if last_announcement.elapsed() >= ANNOUNCE_INTERVAL {
            send_discovery_announcement(socket, store, &announcement);
            last_announcement = Instant::now();
        }
        match socket.recv_from(&mut buffer) {
            Ok((length, source)) => {
                if let Ok(packet) = Packet::decode(&buffer[..length]) {
                    if packet.fingerprint == fingerprint || !is_private_or_loopback(source.ip()) {
                        continue;
                    }
                    let discovered = DiscoveredPeer {
                        fingerprint: packet.fingerprint,
                        label: packet.label,
                        service_endpoint: SocketAddr::new(source.ip(), packet.service_port),
                        pairing_endpoint: SocketAddr::new(source.ip(), packet.pairing_port),
                    };
                    observe_discovered_peer(store, peers, network_issues, &discovered);
                    if packet.kind == ANNOUNCEMENT {
                        // Announcements are sent from short-lived, interface-bound sockets.
                        // Their source port is already closed when our response arrives.
                        send_discovery_response(
                            socket,
                            saved_service_ip(store),
                            &response,
                            discovery_response_address(source),
                        );
                    }
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(_) => thread::sleep(Duration::from_millis(100)),
        }
    }
}

fn discovery_response_address(source: SocketAddr) -> SocketAddr {
    SocketAddr::new(source.ip(), DISCOVERY_PORT)
}

fn observe_discovered_peer(
    store: &DeviceStore,
    peers: &Arc<Mutex<HashMap<CertificateFingerprint, SeenPeer>>>,
    network_issues: &Arc<Mutex<HashMap<CertificateFingerprint, SeenNetworkIssue>>>,
    discovered: &DiscoveredPeer,
) {
    let Some(issue) = discovered_network_issue(discovered) else {
        let now = Instant::now();
        let new_path = peers
            .lock()
            .is_ok_and(|mut known| remember_usable_peer(&mut known, discovered, now));
        if let Ok(mut issues) = network_issues.lock() {
            issues.remove(&discovered.fingerprint);
        }
        if new_path || saved_local_endpoint_is_stale(store, discovered) {
            reconcile_trusted_endpoint(store, discovered);
        }
        return;
    };

    let now = Instant::now();
    let usable_alternative = if let Ok(mut known) = peers.lock() {
        if known
            .get(&discovered.fingerprint)
            .is_some_and(|seen| seen.peer.service_endpoint == discovered.service_endpoint)
        {
            known.remove(&discovered.fingerprint);
        }
        known
            .get(&discovered.fingerprint)
            .is_some_and(|seen| now.saturating_duration_since(seen.last_seen) <= PEER_TTL)
    } else {
        false
    };
    let mut issue_became_active = false;
    if !usable_alternative
        && store.load_peer(discovered.fingerprint).is_ok()
        && let Ok(mut issues) = network_issues.lock()
    {
        issue_became_active = remember_network_issue(&mut issues, issue, now);
    }
    if issue_became_active {
        let _ = crate::tray::reload_existing();
    }
}

fn remember_network_issue(
    issues: &mut HashMap<CertificateFingerprint, SeenNetworkIssue>,
    issue: NetworkPathIssue,
    now: Instant,
) -> bool {
    if let Some(seen) = issues.get_mut(&issue.fingerprint) {
        if seen.issue != issue {
            *seen = SeenNetworkIssue {
                issue,
                first_seen: now,
                last_seen: now,
                active: false,
            };
            return false;
        }
        seen.last_seen = now;
        if !seen.active && now.saturating_duration_since(seen.first_seen) >= NETWORK_ISSUE_GRACE {
            seen.active = true;
            return true;
        }
        return false;
    }
    issues.insert(
        issue.fingerprint,
        SeenNetworkIssue {
            issue,
            first_seen: now,
            last_seen: now,
            active: false,
        },
    );
    false
}

fn remember_usable_peer(
    known: &mut HashMap<CertificateFingerprint, SeenPeer>,
    discovered: &DiscoveredPeer,
    now: Instant,
) -> bool {
    if let Some(seen) = known.get_mut(&discovered.fingerprint) {
        if seen.peer.service_endpoint == discovered.service_endpoint {
            seen.peer.clone_from(discovered);
            seen.last_seen = now;
            return false;
        }
        if now.saturating_duration_since(seen.last_seen) <= PEER_TTL {
            return false;
        }
    }
    known.insert(
        discovered.fingerprint,
        SeenPeer {
            peer: discovered.clone(),
            last_seen: now,
        },
    );
    true
}

fn saved_service_ip(store: &DeviceStore) -> Option<IpAddr> {
    AppSettings::load(store)
        .ok()
        .map(|settings| settings.local_endpoint.ip())
}

fn send_discovery_response(
    fallback: &UdpSocket,
    service_ip: Option<IpAddr>,
    response: &[u8],
    destination: SocketAddr,
) {
    // The packet source is the advertised service address. A wildcard reply can use a
    // different adapter from the listeners, just like an announcement from that adapter.
    if let Some(ip) = service_ip
        && let Ok(socket) = UdpSocket::bind(SocketAddr::new(ip, 0))
    {
        let _ = socket.send_to(response, destination);
        return;
    }
    let _ = fallback.send_to(response, destination);
}

fn announcement_interfaces(
    mut interfaces: Vec<LocalIpv4Interface>,
    service_ip: Option<IpAddr>,
) -> Vec<LocalIpv4Interface> {
    // Services bind one selected address. Advertising another live adapter keeps peers
    // pinned there forever, since its announcements continuously renew the discovery TTL.
    // If that address disappeared, retain all candidates to bootstrap adapter recovery.
    if interfaces
        .iter()
        .any(|interface| Some(IpAddr::V4(interface.address)) == service_ip)
    {
        interfaces.retain(|interface| Some(IpAddr::V4(interface.address)) == service_ip);
    }
    interfaces
}

fn send_discovery_announcement(fallback: &UdpSocket, store: &DeviceStore, announcement: &[u8]) {
    let service_ip = saved_service_ip(store);
    let interfaces = announcement_interfaces(
        local_private_ipv4_interfaces().unwrap_or_default(),
        service_ip,
    );
    let selected_service_is_available = interfaces
        .iter()
        .any(|interface| Some(IpAddr::V4(interface.address)) == service_ip);
    let mut sent = false;
    for interface in interfaces {
        let Some(broadcast) = directed_broadcast(interface.address, interface.prefix_length) else {
            continue;
        };
        let result = UdpSocket::bind(SocketAddr::from((interface.address, 0))).and_then(|socket| {
            socket.set_broadcast(true)?;
            socket.send_to(announcement, SocketAddr::from((broadcast, DISCOVERY_PORT)))?;
            Ok(())
        });
        sent |= result.is_ok();
    }
    if !sent && !selected_service_is_available {
        let _ = fallback.send_to(
            announcement,
            SocketAddr::from((Ipv4Addr::BROADCAST, DISCOVERY_PORT)),
        );
    }
}

fn directed_broadcast(address: Ipv4Addr, prefix_length: u8) -> Option<Ipv4Addr> {
    if !address.is_private() || prefix_length > 30 {
        return None;
    }
    let mask = u32::MAX.checked_shl(u32::from(32 - prefix_length))?;
    Some(Ipv4Addr::from(u32::from(address) | !mask))
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct LocalIpv4Interface {
    address: Ipv4Addr,
    prefix_length: u8,
    adapter_name: String,
}

fn local_private_ipv4_interfaces() -> io::Result<Vec<LocalIpv4Interface>> {
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    let mut byte_length = 15_000_u32;
    loop {
        let word_length = usize::try_from(byte_length)
            .map_err(|_| invalid_data("adapter address buffer is too large"))?
            .div_ceil(size_of::<usize>());
        let mut buffer = vec![0_usize; word_length];
        // SAFETY: the buffer is writable, pointer-aligned, and remains alive while Windows fills
        // and links the adapter records contained within it.
        let result = unsafe {
            GetAdaptersAddresses(
                u32::from(AF_INET.0),
                flags,
                None,
                Some(buffer.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>()),
                &raw mut byte_length,
            )
        };
        if result == ERROR_BUFFER_OVERFLOW.0 {
            continue;
        }
        if result != NO_ERROR.0 {
            return Err(io::Error::from_raw_os_error(
                i32::try_from(result).unwrap_or(i32::MAX),
            ));
        }
        // SAFETY: a successful GetAdaptersAddresses call initialized a linked list entirely
        // inside `buffer`; it stays allocated for the full traversal.
        return Ok(unsafe {
            collect_private_ipv4_interfaces(buffer.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>())
        });
    }
}

unsafe fn collect_private_ipv4_interfaces(
    mut adapter: *const IP_ADAPTER_ADDRESSES_LH,
) -> Vec<LocalIpv4Interface> {
    let mut interfaces = Vec::new();
    while let Some(current) = unsafe { adapter.as_ref() } {
        if current.OperStatus == IfOperStatusUp {
            // SAFETY: FriendlyName points into the GetAdaptersAddresses buffer, which remains
            // alive for this full traversal and contains a null-terminated UTF-16 string.
            let adapter_name = unsafe { current.FriendlyName.to_string() }
                .ok()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| "未命名网络适配器".to_owned());
            let mut unicast = current.FirstUnicastAddress.cast_const();
            while let Some(address) = unsafe { unicast.as_ref() } {
                let socket_address = address.Address;
                if socket_address.iSockaddrLength
                    >= i32::try_from(size_of::<SOCKADDR_IN>()).unwrap_or(i32::MAX)
                    && !socket_address.lpSockaddr.is_null()
                {
                    // SAFETY: the length check above establishes a complete SOCKADDR_IN, and
                    // GetAdaptersAddresses guarantees the pointer lives in the returned buffer.
                    let ipv4 = unsafe {
                        std::ptr::read_unaligned(socket_address.lpSockaddr.cast::<SOCKADDR_IN>())
                    };
                    if ipv4.sin_family == AF_INET {
                        // SAFETY: AF_INET makes the IPv4 byte view of IN_ADDR active.
                        let bytes = unsafe { ipv4.sin_addr.S_un.S_un_b };
                        let value = Ipv4Addr::new(bytes.s_b1, bytes.s_b2, bytes.s_b3, bytes.s_b4);
                        if value.is_private() && address.OnLinkPrefixLength <= 32 {
                            interfaces.push(LocalIpv4Interface {
                                address: value,
                                prefix_length: address.OnLinkPrefixLength,
                                adapter_name: adapter_name.clone(),
                            });
                        }
                    }
                }
                unicast = address.Next.cast_const();
            }
        }
        adapter = current.Next.cast_const();
    }
    interfaces.sort_unstable();
    interfaces.dedup();
    interfaces
}

fn discovered_network_issue(peer: &DiscoveredPeer) -> Option<NetworkPathIssue> {
    let IpAddr::V4(peer_address) = peer.service_endpoint.ip() else {
        return None;
    };
    let interfaces = local_private_ipv4_interfaces().ok()?;
    network_issue_for_interfaces(peer, peer_address, &interfaces)
}

fn network_issue_for_interfaces(
    peer: &DiscoveredPeer,
    peer_address: Ipv4Addr,
    interfaces: &[LocalIpv4Interface],
) -> Option<NetworkPathIssue> {
    let owner = interfaces
        .iter()
        .find(|interface| interface.address == peer_address)?;
    Some(NetworkPathIssue {
        fingerprint: peer.fingerprint,
        peer_label: peer.label.clone(),
        peer_endpoint: peer.service_endpoint,
        local_adapter: owner.adapter_name.clone(),
    })
}

/// Verifies that saved LAN endpoints still describe two different machines on an active route.
///
/// Loopback endpoints are accepted for bounded local diagnostics. Private IPv4 endpoints must use
/// an address currently assigned to this computer, while the peer address must not be assigned to
/// any local adapter. This catches Mobile Hotspot, VPN, Hyper-V, and similar address collisions
/// before either product listener binds the wrong interface.
///
/// # Errors
///
/// Returns an actionable error when adapter enumeration fails, the saved local address disappeared,
/// or the supposed peer address is also owned by this computer.
pub fn validate_saved_network_path(settings: &AppSettings) -> io::Result<()> {
    if settings.local_endpoint.ip().is_loopback() && settings.peer_endpoint.ip().is_loopback() {
        return Ok(());
    }
    let (IpAddr::V4(local_address), IpAddr::V4(peer_address)) =
        (settings.local_endpoint.ip(), settings.peer_endpoint.ip())
    else {
        return Ok(());
    };
    let interfaces = local_private_ipv4_interfaces()?;
    validate_ipv4_path(local_address, peer_address, &interfaces)
}

fn validate_ipv4_path(
    local_address: Ipv4Addr,
    peer_address: Ipv4Addr,
    interfaces: &[LocalIpv4Interface],
) -> io::Result<()> {
    if let Some(owner) = interfaces
        .iter()
        .find(|interface| interface.address == peer_address)
    {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!(
                "对端地址 {peer_address} 同时属于本机网卡“{}”。Pierdesk 不会使用或保存这条冲突路径；请关闭冲突的本机热点或虚拟网卡，或让两台电脑连接同一局域网。冲突解除后会自动恢复",
                owner.adapter_name
            ),
        ));
    }
    if !interfaces
        .iter()
        .any(|interface| interface.address == local_address)
    {
        return Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            format!(
                "已保存的本机地址 {local_address} 当前不属于任何已连接网卡。Pierdesk 正在通过自动发现寻找新路径，找到后会自动恢复"
            ),
        ));
    }
    Ok(())
}

/// A path also changes when only this computer switched adapters (for example Wi-Fi to Ethernet)
/// while the peer kept its address. The peer's announcement is then unchanged, so without this
/// check the saved local endpoint kept pointing at the vanished adapter until a restart.
fn saved_local_endpoint_is_stale(store: &DeviceStore, peer: &DiscoveredPeer) -> bool {
    let Ok(settings) = AppSettings::load(store) else {
        return false;
    };
    route_local_ip(peer.service_endpoint)
        .is_ok_and(|route_ip| local_endpoint_differs_from_route(&settings, peer, route_ip))
}

fn local_endpoint_differs_from_route(
    settings: &AppSettings,
    peer: &DiscoveredPeer,
    route_ip: IpAddr,
) -> bool {
    settings.active_peer == peer.fingerprint && settings.local_endpoint.ip() != route_ip
}

fn reconcile_trusted_endpoint(store: &DeviceStore, peer: &DiscoveredPeer) {
    if store.load_peer(peer.fingerprint).is_err() {
        return;
    }
    let existing = AppSettings::load(store).ok();
    if existing
        .as_ref()
        .is_some_and(|settings| settings.active_peer != peer.fingerprint)
    {
        return;
    }
    let Ok(local_ip) = route_local_ip(peer.service_endpoint) else {
        return;
    };
    let settings = AppSettings {
        local_endpoint: SocketAddr::new(local_ip, SERVICE_PORT),
        active_peer: peer.fingerprint,
        peer_endpoint: peer.service_endpoint,
        auto_receive: existing
            .as_ref()
            .is_none_or(|settings| settings.auto_receive),
    };
    if existing.as_ref() == Some(&settings) {
        let _ = crate::tray::reload_existing();
        return;
    }
    if settings.save(store).is_err() {
        return;
    }
    let _ = crate::tray::reload_existing();
}

/// Persists one already-trusted discovered peer as the active automatic LAN connection.
///
/// # Errors
///
/// Returns an error when the trust record is missing, no private route exists, or settings cannot
/// be validated and saved.
pub fn activate_discovered_peer(store: &DeviceStore, peer: &DiscoveredPeer) -> io::Result<()> {
    store.load_peer(peer.fingerprint)?;
    let local_ip = route_local_ip(peer.service_endpoint)?;
    AppSettings {
        local_endpoint: SocketAddr::new(local_ip, SERVICE_PORT),
        active_peer: peer.fingerprint,
        peer_endpoint: peer.service_endpoint,
        auto_receive: true,
    }
    .save(store)
}

fn route_local_ip(peer: SocketAddr) -> io::Result<IpAddr> {
    if let IpAddr::V4(peer_address) = peer.ip()
        && let Some(owner) = local_private_ipv4_interfaces()?
            .into_iter()
            .find(|interface| interface.address == peer_address)
    {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!(
                "the discovered peer address {peer_address} belongs to local adapter {:?}",
                owner.adapter_name
            ),
        ));
    }
    let bind = match peer {
        SocketAddr::V4(_) => SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
        SocketAddr::V6(_) => "[::]:0".parse().expect("valid IPv6 wildcard endpoint"),
    };
    let socket = UdpSocket::bind(bind)?;
    socket.connect(peer)?;
    let address = socket.local_addr()?.ip();
    if is_private_or_loopback(address) {
        Ok(address)
    } else {
        Err(invalid_data(
            "the discovered peer has no private local route",
        ))
    }
}

fn local_device_label() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .map_or_else(|| "Windows 设备".to_owned(), |label| bounded_label(&label))
}

fn bounded_label(label: &str) -> String {
    let cleaned = label
        .trim()
        .chars()
        .filter(|character| !character.is_control())
        .collect::<String>();
    let source = if cleaned.is_empty() {
        "Windows 设备"
    } else {
        &cleaned
    };
    let mut result = String::new();
    for character in source.chars() {
        if result.len() + character.len_utf8() > LABEL_LIMIT {
            break;
        }
        result.push(character);
    }
    result
}

fn is_private_or_loopback(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => address.is_loopback() || address.is_private(),
        IpAddr::V6(address) => address.is_loopback() || address.is_unique_local(),
    }
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_round_trip_is_strict() {
        let fingerprint = CertificateFingerprint::from_certificate(b"peer");
        let packet = Packet::new(ANNOUNCEMENT, fingerprint, "测试电脑-🚢");
        let encoded = packet.encode().unwrap();
        assert_eq!(Packet::decode(&encoded).unwrap(), packet);

        let mut trailing = encoded.clone();
        trailing.push(0);
        assert!(Packet::decode(&trailing).is_err());
        assert!(Packet::decode(&encoded[..HEADER_LENGTH - 1]).is_err());

        let mut wrong_port = encoded;
        wrong_port[43..45].copy_from_slice(&45_999_u16.to_le_bytes());
        assert!(Packet::decode(&wrong_port).is_err());
    }

    #[test]
    fn bounded_label_preserves_utf8_boundaries() {
        let label = bounded_label(&format!("{}🚢", "a".repeat(LABEL_LIMIT - 1)));
        assert!(label.len() <= LABEL_LIMIT);
        assert!(label.is_char_boundary(label.len()));
    }

    #[test]
    fn local_adapter_switch_is_a_new_path_even_when_the_peer_address_is_unchanged() {
        // 2026-10-11: 上笔 moved from Wi-Fi .11 to Ethernet .13 while 下脑 stayed at .38.
        let peer_fingerprint = CertificateFingerprint::from_certificate(b"desktop");
        let peer = DiscoveredPeer {
            fingerprint: peer_fingerprint,
            label: "desktop".to_owned(),
            service_endpoint: "192.168.1.38:45233".parse().unwrap(),
            pairing_endpoint: "192.168.1.38:45232".parse().unwrap(),
        };
        let settings = AppSettings {
            local_endpoint: "192.168.1.11:45233".parse().unwrap(),
            active_peer: peer_fingerprint,
            peer_endpoint: peer.service_endpoint,
            auto_receive: true,
        };
        assert!(local_endpoint_differs_from_route(
            &settings,
            &peer,
            "192.168.1.13".parse().unwrap()
        ));
        assert!(!local_endpoint_differs_from_route(
            &settings,
            &peer,
            "192.168.1.11".parse().unwrap()
        ));
        let other = DiscoveredPeer {
            fingerprint: CertificateFingerprint::from_certificate(b"other"),
            ..peer
        };
        assert!(!local_endpoint_differs_from_route(
            &settings,
            &other,
            "192.168.1.13".parse().unwrap()
        ));
    }

    #[test]
    fn discovery_rejects_public_sources() {
        assert!(is_private_or_loopback("127.0.0.1".parse().unwrap()));
        assert!(is_private_or_loopback("192.168.1.2".parse().unwrap()));
        assert!(!is_private_or_loopback("8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn interface_announcement_reply_targets_the_persistent_listener() {
        for source in ["192.168.1.29:50123", "192.168.137.1:61234"] {
            let source: SocketAddr = source.parse().unwrap();
            let destination = discovery_response_address(source);
            assert_eq!(destination.ip(), source.ip());
            assert_eq!(destination.port(), DISCOVERY_PORT);
            assert_ne!(destination.port(), source.port());
        }
    }

    #[test]
    fn dual_adapter_announcements_only_advertise_the_service_listener() {
        let wifi = test_interface("192.168.1.11", "Wi-Fi");
        let ethernet = test_interface("192.168.1.13", "Ethernet");
        assert_eq!(
            announcement_interfaces(
                vec![wifi, ethernet.clone()],
                Some("192.168.1.13".parse().unwrap()),
            ),
            vec![ethernet]
        );
    }

    #[test]
    fn missing_service_adapter_keeps_discovery_candidates_for_recovery() {
        let interfaces = vec![test_interface("192.168.1.13", "Ethernet")];
        for saved in [None, Some("192.168.1.11".parse().unwrap())] {
            assert_eq!(
                announcement_interfaces(interfaces.clone(), saved),
                interfaces
            );
        }
    }

    #[test]
    fn discovery_reply_source_matches_the_service_listener() {
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let fallback = UdpSocket::bind("0.0.0.0:0").unwrap();
        send_discovery_response(
            &fallback,
            Some("127.0.0.2".parse().unwrap()),
            b"service-address",
            receiver.local_addr().unwrap(),
        );
        let mut buffer = [0_u8; 32];
        let (length, source) = receiver.recv_from(&mut buffer).unwrap();
        assert_eq!(&buffer[..length], b"service-address");
        assert_eq!(source.ip(), "127.0.0.2".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn directed_broadcast_uses_each_interface_prefix() {
        assert_eq!(
            directed_broadcast("192.168.137.1".parse().unwrap(), 24),
            Some("192.168.137.255".parse().unwrap())
        );
        assert_eq!(
            directed_broadcast("172.20.144.1".parse().unwrap(), 20),
            Some("172.20.159.255".parse().unwrap())
        );
        assert_eq!(directed_broadcast("8.8.8.8".parse().unwrap(), 24), None);
        assert_eq!(
            directed_broadcast("192.168.137.1".parse().unwrap(), 31),
            None
        );
    }

    #[test]
    fn adapter_enumeration_returns_only_usable_private_ipv4_addresses() {
        for interface in local_private_ipv4_interfaces().unwrap() {
            assert!(interface.address.is_private());
            assert!(interface.prefix_length <= 32);
            assert!(!interface.adapter_name.is_empty());
        }
    }

    #[test]
    fn address_collision_is_rejected_before_route_selection() {
        let fingerprint = CertificateFingerprint::from_certificate(b"trusted peer");
        let peer = DiscoveredPeer {
            fingerprint,
            label: "主机".to_owned(),
            service_endpoint: "192.168.137.1:45233".parse().unwrap(),
            pairing_endpoint: "192.168.137.1:45232".parse().unwrap(),
        };
        let interfaces = vec![
            test_interface("192.168.137.12", "WLAN"),
            test_interface("192.168.137.1", "本地连接* 2"),
        ];

        let issue =
            network_issue_for_interfaces(&peer, "192.168.137.1".parse().unwrap(), &interfaces)
                .unwrap();
        assert_eq!(issue.local_adapter, "本地连接* 2");
        assert!(issue.user_message().contains("已保留原配置"));
        let error = validate_ipv4_path(
            "192.168.137.12".parse().unwrap(),
            "192.168.137.1".parse().unwrap(),
            &interfaces,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
    }

    #[test]
    fn valid_distinct_route_is_accepted_and_stale_local_address_is_rejected() {
        let interfaces = vec![test_interface("192.168.137.12", "WLAN")];
        assert!(
            validate_ipv4_path(
                "192.168.137.12".parse().unwrap(),
                "192.168.137.1".parse().unwrap(),
                &interfaces,
            )
            .is_ok()
        );
        let error = validate_ipv4_path(
            "192.168.1.44".parse().unwrap(),
            "192.168.137.1".parse().unwrap(),
            &interfaces,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AddrNotAvailable);
    }

    #[test]
    fn viable_candidate_suppresses_conflict_from_another_adapter() {
        let fingerprint = CertificateFingerprint::from_certificate(b"peer");
        let peer = DiscoveredPeer {
            fingerprint,
            label: "peer".to_owned(),
            service_endpoint: "192.168.1.20:45233".parse().unwrap(),
            pairing_endpoint: "192.168.1.20:45232".parse().unwrap(),
        };
        let issue = NetworkPathIssue {
            fingerprint,
            peer_label: "peer".to_owned(),
            peer_endpoint: "192.168.137.1:45233".parse().unwrap(),
            local_adapter: "本地连接* 2".to_owned(),
        };
        let view = DiscoveryView {
            peers: Arc::new(Mutex::new(HashMap::from([(
                fingerprint,
                SeenPeer {
                    peer,
                    last_seen: Instant::now(),
                },
            )]))),
            network_issues: Arc::new(Mutex::new(HashMap::from([(
                fingerprint,
                SeenNetworkIssue {
                    issue,
                    first_seen: Instant::now(),
                    last_seen: Instant::now(),
                    active: true,
                },
            )]))),
        };

        assert!(view.network_issue(fingerprint).is_none());
        assert!(view.peer(fingerprint).is_some());
    }

    #[test]
    fn fresh_working_candidate_is_stable_across_duplicate_networks() {
        let fingerprint = CertificateFingerprint::from_certificate(b"peer");
        let first = test_peer(fingerprint, "192.168.1.20");
        let second = test_peer(fingerprint, "10.20.30.40");
        let now = Instant::now();
        let mut known = HashMap::new();

        assert!(remember_usable_peer(&mut known, &first, now));
        assert!(!remember_usable_peer(&mut known, &second, now));
        assert_eq!(
            known.get(&fingerprint).unwrap().peer.service_endpoint,
            first.service_endpoint
        );

        let expired = now
            .checked_add(PEER_TTL + Duration::from_millis(1))
            .unwrap();
        assert!(remember_usable_peer(&mut known, &second, expired));
        assert_eq!(
            known.get(&fingerprint).unwrap().peer.service_endpoint,
            second.service_endpoint
        );
    }

    #[test]
    fn address_conflict_must_persist_before_it_becomes_user_visible() {
        let fingerprint = CertificateFingerprint::from_certificate(b"peer");
        let issue = NetworkPathIssue {
            fingerprint,
            peer_label: "peer".to_owned(),
            peer_endpoint: "192.168.137.1:45233".parse().unwrap(),
            local_adapter: "本地连接* 2".to_owned(),
        };
        let first_seen = Instant::now();
        let mut issues = HashMap::new();

        assert!(!remember_network_issue(
            &mut issues,
            issue.clone(),
            first_seen
        ));
        assert!(!issues.get(&fingerprint).unwrap().active);
        assert!(!remember_network_issue(
            &mut issues,
            issue.clone(),
            (first_seen + NETWORK_ISSUE_GRACE)
                .checked_sub(Duration::from_millis(1))
                .unwrap()
        ));
        assert!(remember_network_issue(
            &mut issues,
            issue,
            first_seen + NETWORK_ISSUE_GRACE
        ));
        assert!(issues.get(&fingerprint).unwrap().active);
    }

    fn test_interface(address: &str, adapter_name: &str) -> LocalIpv4Interface {
        LocalIpv4Interface {
            address: address.parse().unwrap(),
            prefix_length: 24,
            adapter_name: adapter_name.to_owned(),
        }
    }

    fn test_peer(fingerprint: CertificateFingerprint, address: &str) -> DiscoveredPeer {
        let address = address.parse::<IpAddr>().unwrap();
        DiscoveredPeer {
            fingerprint,
            label: "peer".to_owned(),
            service_endpoint: SocketAddr::new(address, SERVICE_PORT),
            pairing_endpoint: SocketAddr::new(address, PAIRING_PORT),
        }
    }
}
