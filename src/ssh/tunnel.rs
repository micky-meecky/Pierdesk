//! Bounded SSH byte tunnels. Network addresses are hints; TLS and the grant are authority.
use super::*;
use std::fs::{File, OpenOptions};
use std::net::{Shutdown, TcpStream};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use windows::Win32::{
    Foundation::{ERROR_PIPE_CONNECTED, ERROR_PIPE_LISTENING, HANDLE, HLOCAL, LocalFree},
    Security::{
        Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1},
        PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
    },
    Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX},
    System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeServerProcessId, PIPE_NOWAIT,
        PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PeekNamedPipe,
        SetNamedPipeHandleState,
    },
};
use windows_core::PCWSTR;

const MAX_CONNECTIONS: usize = 32;
const MAX_PEER_TUNNELS: usize = 8;
const BUFFER_SIZE: usize = 65536;

#[derive(Clone)]
pub(super) struct LiveTunnel {
    pub session: ActiveSession,
    pub cancel: Arc<AtomicBool>,
}
struct SessionGuard {
    shared: Arc<Mutex<Snapshot>>,
    id: String,
}
impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.shared
            .lock()
            .unwrap()
            .tunnels
            .retain(|t| t.session.id != self.id);
    }
}
fn session(
    shared: &Arc<Mutex<Snapshot>>,
    connection_id: &str,
    peer: &str,
    direction: &str,
    address: &str,
) -> io::Result<(SessionGuard, Arc<AtomicBool>)> {
    let mut state = shared.lock().unwrap();
    if state.tunnels.len() >= MAX_CONNECTIONS
        || state
            .tunnels
            .iter()
            .filter(|t| t.session.peer == peer)
            .count()
            >= MAX_PEER_TUNNELS
    {
        return Err(invalid(
            "SSH channel limit reached; close an unused session",
        ));
    }
    let id = random_id()?;
    let cancel = Arc::new(AtomicBool::new(false));
    state.tunnels.push(LiveTunnel {
        session: ActiveSession {
            id: id.clone(),
            connection_id: connection_id.into(),
            peer: peer.into(),
            direction: direction.into(),
            address: address.into(),
            port: CONTROL_PORT,
            started: now(),
        },
        cancel: cancel.clone(),
    });
    Ok((
        SessionGuard {
            shared: shared.clone(),
            id,
        },
        cancel,
    ))
}
fn transient(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}
struct Pending {
    bytes: Vec<u8>,
    position: usize,
    eof: bool,
    flushed: bool,
}
impl Pending {
    fn new() -> Self {
        Self {
            bytes: vec![],
            position: 0,
            eof: false,
            flushed: true,
        }
    }
    fn step(
        &mut self,
        source: &mut (impl Read + Write),
        target: &mut (impl Read + Write),
    ) -> io::Result<bool> {
        let mut progress = false;
        if self.position == self.bytes.len() && !self.eof {
            self.bytes.resize(BUFFER_SIZE, 0);
            match source.read(&mut self.bytes) {
                Ok(n) => {
                    self.bytes.truncate(n);
                    self.position = 0;
                    self.eof = n == 0;
                    progress = n > 0;
                }
                Err(e) if transient(&e) => {
                    self.bytes.clear();
                    self.position = 0;
                }
                Err(e) => return Err(e),
            }
        }
        if self.position < self.bytes.len() {
            match target.write(&self.bytes[self.position..]) {
                // A bounded TLS writer can temporarily accept zero plaintext bytes when
                // encrypted records occupy its send buffer. Keep this chunk and flush below;
                // treating backpressure as EOF breaks large SFTP uploads.
                Ok(0) => {}
                Ok(n) => {
                    self.position += n;
                    self.flushed = false;
                    progress = true;
                }
                Err(e) if transient(&e) => {}
                Err(e) => return Err(e),
            }
        }
        match target.flush() {
            Ok(()) => self.flushed = true,
            Err(e) if transient(&e) => self.flushed = false,
            Err(e) => return Err(e),
        }
        Ok(progress)
    }
    fn drained(&self) -> bool {
        self.position == self.bytes.len() && self.flushed
    }
}
fn bridge(
    left: &mut (impl Read + Write),
    right: &mut (impl Read + Write),
    stop: &AtomicBool,
    cancel: &AtomicBool,
) -> io::Result<()> {
    let (mut a, mut b) = (Pending::new(), Pending::new());
    let mut drain_started = None;
    while !stop.load(Ordering::Acquire) && !cancel.load(Ordering::Acquire) {
        let progress = a.step(left, right)? | b.step(right, left)?;
        if a.eof || b.eof {
            if a.drained() && b.drained() {
                break;
            }
            if drain_started.get_or_insert_with(Instant::now).elapsed() > Duration::from_secs(5) {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "SSH channel output did not drain",
                ));
            }
        }
        if !progress {
            thread::sleep(Duration::from_millis(4));
        }
    }
    Ok(())
}
pub(super) fn validate_grant(state: &Snapshot, peer: &str, request: &Request) -> io::Result<()> {
    let key = platform::validate_public_key(&request.key)?;
    if request.version != CONTROL_VERSION
        || !valid_id(&request.id)
        || !state.data.incoming.iter().any(|g| {
            g.peer == peer
                && g.id == request.id
                && g.public_key == key
                && g.status == "authorized"
                && g.account.is_some()
        })
    {
        return Err(invalid(
            "SSH access is not authorized for this device and key",
        ));
    }
    if !state.server.loopback_only {
        return Err(invalid(
            "SSH access upgrade required on the target computer",
        ));
    }
    if !state.server.running {
        return Err(invalid(
            "Target SSH service is stopped; repair local access on the target computer",
        ));
    }
    Ok(())
}
pub(super) fn receive(
    store: &DeviceStore,
    shared: &Arc<Mutex<Snapshot>>,
    connection: crate::security::AuthenticatedServerConnection,
    request: Request,
    stop: &AtomicBool,
) -> io::Result<()> {
    receive_at(
        store,
        shared,
        connection,
        request,
        stop,
        SocketAddr::from(([127, 0, 0, 1], DEFAULT_SSH_PORT)),
    )
}
// The destination is supplied only by the fixed production wrapper, never by a wire request.
#[allow(clippy::needless_pass_by_value)] // Own the request for the lifetime of this connection worker.
fn receive_at(
    store: &DeviceStore,
    shared: &Arc<Mutex<Snapshot>>,
    mut connection: crate::security::AuthenticatedServerConnection,
    request: Request,
    stop: &AtomicBool,
    destination: SocketAddr,
) -> io::Result<()> {
    let peer = connection.peer_fingerprint.to_string();
    let prepared = (|| {
        store.load_peer(connection.peer_fingerprint)?;
        validate_grant(&shared.lock().unwrap(), &peer, &request)?;
        let ssh = TcpStream::connect_timeout(&destination, Duration::from_secs(3))?;
        let address = connection.stream.sock.peer_addr()?.ip().to_string();
        let (guard, cancel) = session(shared, &request.id, &peer, "incoming", &address)?;
        store.load_peer(connection.peer_fingerprint)?;
        validate_grant(&shared.lock().unwrap(), &peer, &request)?;
        Ok::<_, io::Error>((ssh, guard, cancel))
    })();
    let (mut ssh, _guard, cancel) = match prepared {
        Ok(value) => value,
        Err(error) => {
            write_frame(
                &mut connection.stream,
                &serde_json::json!({"version":CONTROL_VERSION,"status":"error","account":null,"port":0,"keys":[],"error":error.to_string()}),
            )?;
            connection.stream.conn.send_close_notify();
            let _ = connection.stream.flush();
            return Ok(());
        }
    };
    write_frame(
        &mut connection.stream,
        &serde_json::json!({"version":CONTROL_VERSION,"status":"tunnel-ready","account":null,"port":DEFAULT_SSH_PORT,"keys":[]}),
    )?;
    connection.stream.conn.set_buffer_limit(Some(BUFFER_SIZE));
    connection.stream.sock.set_nonblocking(true)?;
    ssh.set_nonblocking(true)?;
    let result = bridge(&mut connection.stream, &mut ssh, stop, &cancel);
    let _ = ssh.shutdown(Shutdown::Both);
    let _ = connection.stream.sock.shutdown(Shutdown::Both);
    result
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalRequest {
    #[serde(default)]
    action: String,
    id: String,
}
#[derive(Serialize, Deserialize)]
struct Endpoint {
    pid: u32,
    executable: PathBuf,
}
fn pipe_name(store: &DeviceStore) -> io::Result<String> {
    Ok(format!(
        r"\\.\pipe\ClipFerry-SSH-{}",
        store.load_identity()?.fingerprint()
    ))
}
struct Pipe(File);
impl Pipe {
    fn handle(&self) -> HANDLE {
        HANDLE(self.0.as_raw_handle())
    }
}
impl Read for Pipe {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let mut available = 0;
        // SAFETY: owned live pipe, query-only call and valid output buffer.
        match unsafe { PeekNamedPipe(self.handle(), None, 0, None, Some(&raw mut available), None) }
        {
            Ok(()) if available == 0 => Err(io::ErrorKind::WouldBlock.into()),
            Ok(()) => self.0.read(bytes),
            Err(e) if matches!(e.code().0.cast_unsigned() & 0xffff, 109 | 233) => Ok(0),
            Err(e) => Err(io::Error::other(e)),
        }
    }
}
impl Write for Pipe {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self.0.write(bytes) {
            Ok(0) if !bytes.is_empty() => Err(io::ErrorKind::WouldBlock.into()),
            Err(e) if e.raw_os_error() == Some(232) => Err(io::ErrorKind::WouldBlock.into()),
            result => result,
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn create_pipe(name: &str, first: bool) -> io::Result<Pipe> {
    let sid = crate::autostart::current_sid().map_err(io::Error::other)?;
    let sddl: Vec<u16> = format!("D:P(A;;GA;;;{sid})(A;;GA;;;SY)")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    // SAFETY: descriptor is freed after CreateNamedPipe copies it; File owns the resulting handle.
    unsafe {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &raw mut descriptor,
            None,
        )
        .map_err(io::Error::other)?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).map_err(io::Error::other)?,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: false.into(),
        };
        let flags = if first {
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE
        } else {
            PIPE_ACCESS_DUPLEX
        };
        let handle = CreateNamedPipeW(
            PCWSTR(name.as_ptr()),
            flags,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_NOWAIT | PIPE_REJECT_REMOTE_CLIENTS,
            u32::try_from(MAX_CONNECTIONS + 2).map_err(io::Error::other)?,
            u32::try_from(BUFFER_SIZE).map_err(io::Error::other)?,
            u32::try_from(BUFFER_SIZE).map_err(io::Error::other)?,
            0,
            Some(&raw const attributes),
        );
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
        if handle.is_invalid() {
            return Err(io::Error::last_os_error());
        }
        Ok(Pipe(File::from_raw_handle(handle.0)))
    }
}
fn bounded_frame<T: for<'de> Deserialize<'de>>(
    pipe: &mut Pipe,
    stop: &AtomicBool,
) -> io::Result<T> {
    struct Wait<'a> {
        pipe: &'a mut Pipe,
        stop: &'a AtomicBool,
        deadline: Instant,
    }
    impl Read for Wait<'_> {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            while !self.stop.load(Ordering::Acquire) && Instant::now() < self.deadline {
                match self.pipe.read(b) {
                    Err(e) if transient(&e) => thread::sleep(Duration::from_millis(4)),
                    result => return result,
                }
            }
            Err(io::ErrorKind::TimedOut.into())
        }
    }
    read_frame_payload(&mut Wait {
        pipe,
        stop,
        deadline: Instant::now() + Duration::from_secs(25),
    })
}
fn outgoing(
    store: &DeviceStore,
    discovery: Option<&DiscoveryView>,
    shared: &Arc<Mutex<Snapshot>>,
    pipe: &mut Pipe,
    stop: &AtomicBool,
    started: &mut bool,
) -> io::Result<()> {
    let request: LocalRequest = bounded_frame(pipe, stop)?;
    if request.action == "status" {
        let info = platform::inspect(&store.root().join("ssh"))?;
        shared.lock().unwrap().server = info.clone();
        let state = shared.lock().unwrap();
        return write_frame(
            pipe,
            &serde_json::json!({"ok":true,"server":info,"sessions":state.tunnels.iter().map(|t| &t.session).collect::<Vec<_>>(),"listenerError":state.listener_error,"localProxyError":state.local_proxy_error,"remoteAccess":state.remote_access}),
        );
    }
    if !request.action.is_empty() && request.action != "tunnel" {
        return Err(invalid("Unsupported local SSH operation"));
    }
    let c = shared
        .lock()
        .unwrap()
        .data
        .outgoing
        .iter()
        .find(|c| c.id == request.id)
        .cloned()
        .ok_or_else(|| invalid("SSH connection not found"))?;
    if !matches!(c.status.as_str(), "authorized" | "ready" | "offline")
        || c.account.is_none()
        || c.host_keys.is_empty()
        || !c.offered_keys.is_empty()
    {
        return Err(invalid(
            "SSH connection is not authorized or requires identity review",
        ));
    }
    let fingerprint = CertificateFingerprint::from_str(&c.peer)?;
    let peer = store.load_peer(fingerprint)?;
    let address = peer_address(discovery, fingerprint, Some(&c.address))?;
    let client = PinnedTlsClient::new(
        &store.load_identity()?,
        peer.into_certificate_der(),
        fingerprint,
        Duration::from_secs(3),
    )?;
    let mut remote = client.connect(address)?;
    write_frame(
        &mut remote,
        &Request {
            version: CONTROL_VERSION,
            action: "tunnel".into(),
            id: c.id.clone(),
            key: c.public_key.clone(),
        },
    )?;
    let response: Response = read_frame(&mut remote)?;
    if response.version != CONTROL_VERSION || response.status != "tunnel-ready" {
        return Err(invalid(if response.error.is_empty() {
            "Target SSH access upgrade required".into()
        } else {
            response.error
        }));
    }
    store.load_peer(fingerprint)?;
    let (_guard, cancel) = session(
        shared,
        &c.id,
        &c.peer,
        "outgoing",
        &address.ip().to_string(),
    )?;
    write_frame(pipe, &serde_json::json!({"ok":true}))?;
    *started = true;
    remote.conn.set_buffer_limit(Some(BUFFER_SIZE));
    remote.sock.set_nonblocking(true)?;
    let result = bridge(pipe, &mut remote, stop, &cancel);
    let _ = remote.sock.shutdown(Shutdown::Both);
    result
}
#[allow(clippy::needless_pass_by_value)] // These resources belong to the dedicated listener worker.
pub(super) fn serve_local(
    store: DeviceStore,
    discovery: Option<DiscoveryView>,
    shared: Arc<Mutex<Snapshot>>,
    stop: Arc<AtomicBool>,
) -> io::Result<()> {
    let name = pipe_name(&store)?;
    let mut pipe = create_pipe(&name, true)?;
    let endpoint = store.root().join("ssh/proxy-runtime.json");
    fs::write(
        &endpoint,
        serde_json::to_vec(&Endpoint {
            pid: std::process::id(),
            executable: std::env::current_exe()?,
        })
        .map_err(io::Error::other)?,
    )?;
    let mut workers: Vec<JoinHandle<()>> = vec![];
    while !stop.load(Ordering::Acquire) {
        workers.retain(|w| !w.is_finished());
        // SAFETY: owned pipe in nonblocking listen mode, without overlapped IO.
        let connected = match unsafe { ConnectNamedPipe(pipe.handle(), None) } {
            Ok(()) => true,
            Err(e) if e.code() == windows_core::HRESULT::from_win32(ERROR_PIPE_CONNECTED.0) => true,
            Err(e) if e.code() == windows_core::HRESULT::from_win32(ERROR_PIPE_LISTENING.0) => {
                false
            }
            Err(e) => return Err(io::Error::other(e)),
        };
        if connected {
            let next = create_pipe(&name, false)?;
            let mut connected_pipe = std::mem::replace(&mut pipe, next);
            if workers.len() >= MAX_CONNECTIONS {
                continue;
            }
            let (store, discovery, shared, stop) = (
                store.clone(),
                discovery.clone(),
                shared.clone(),
                stop.clone(),
            );
            workers.push(thread::spawn(move || {
                let mut started = false;
                if let Err(error) = outgoing(
                    &store,
                    discovery.as_ref(),
                    &shared,
                    &mut connected_pipe,
                    &stop,
                    &mut started,
                ) {
                    if !started {
                        let _ = write_frame(
                            &mut connected_pipe,
                            &serde_json::json!({"ok":false,"error":error.to_string()}),
                        );
                    }
                    append_ssh_diagnostic(
                        &store,
                        &format!(
                            "{} proxy result=failed details={}",
                            now(),
                            diagnostic_error(&error.to_string())
                        ),
                    );
                }
            }));
        } else {
            thread::sleep(Duration::from_millis(15));
        }
    }
    let _ = fs::remove_file(endpoint);
    for worker in workers {
        let _ = worker.join();
    }
    Ok(())
}
fn connect_runtime(store: &DeviceStore) -> io::Result<File> {
    let endpoint: Endpoint = serde_json::from_slice(
        &fs::read(store.root().join("ssh/proxy-runtime.json"))
            .map_err(|_| invalid("Open Pierdesk on this computer before connecting"))?,
    )
    .map_err(io::Error::other)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    let name = pipe_name(store)?;
    let file = loop {
        match OpenOptions::new().read(true).write(true).open(&name) {
            Ok(f) => break f,
            Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            Err(_) => return Err(invalid("Open Pierdesk on this computer before connecting")),
        }
    };
    let mut pid = 0;
    // SAFETY: owned pipe and valid output; verify its owner before sending any bytes.
    unsafe { GetNamedPipeServerProcessId(HANDLE(file.as_raw_handle()), &raw mut pid) }
        .map_err(io::Error::other)?;
    if pid != endpoint.pid || platform::process_image(pid)? != endpoint.executable {
        return Err(invalid("Pierdesk local proxy identity mismatch"));
    }
    // Blocking client IO is confined to the helper process. The application uses cancellable IO.
    unsafe {
        SetNamedPipeHandleState(
            HANDLE(file.as_raw_handle()),
            Some(&PIPE_READMODE_BYTE),
            None,
            None,
        )
    }
    .map_err(io::Error::other)?;
    Ok(file)
}

pub(super) fn runtime_status() -> io::Result<serde_json::Value> {
    let store = DeviceStore::current_user()?;
    let mut pipe = Pipe(connect_runtime(&store)?);
    write_frame(
        &mut pipe,
        &LocalRequest {
            id: String::new(),
            action: "status".into(),
        },
    )?;
    bounded_frame(&mut pipe, &AtomicBool::new(false))
}

/// OpenSSH `ProxyCommand` entry point. stdout contains only SSH protocol bytes.
pub fn run_proxy(id: &str) -> io::Result<()> {
    if !valid_id(id) {
        return Err(invalid("Invalid SSH connection identity"));
    }
    let store = DeviceStore::current_user()?;
    let mut pipe = Pipe(connect_runtime(&store)?);
    write_frame(
        &mut pipe,
        &LocalRequest {
            id: id.into(),
            action: "tunnel".into(),
        },
    )?;
    let response: serde_json::Value = bounded_frame(&mut pipe, &AtomicBool::new(false))?;
    if response["ok"] != true {
        return Err(invalid(
            response["error"]
                .as_str()
                .unwrap_or("SSH proxy could not connect"),
        ));
    }
    let mut writer = pipe.0.try_clone()?;
    let input_done = Arc::new(AtomicBool::new(false));
    let finished = input_done.clone();
    thread::spawn(move || {
        let _ = io::copy(&mut io::stdin().lock(), &mut writer);
        finished.store(true, Ordering::Release);
    });
    let mut stdout = io::stdout().lock();
    let mut bytes = [0_u8; 16384];
    while !input_done.load(Ordering::Acquire) {
        match pipe.read(&mut bytes) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                stdout.write_all(&bytes[..n])?;
                // Stdout is line buffered even for a GUI helper. Encrypted SSH records may
                // contain no newline; flush each chunk to avoid stalling authentication.
                stdout.flush()?;
            }
            Err(e) if transient(&e) => thread::sleep(Duration::from_millis(4)),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                ) =>
            {
                return Ok(());
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn saturated_writer_retains_bytes_until_flush_makes_room() {
        struct Saturated {
            full: bool,
            received: Vec<u8>,
        }
        impl Read for Saturated {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::ErrorKind::WouldBlock.into())
            }
        }
        impl Write for Saturated {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.full {
                    return Ok(0);
                }
                self.received.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                self.full = false;
                Ok(())
            }
        }
        let bytes = vec![0, 255, 128, 1];
        let mut source = io::Cursor::new(bytes.clone());
        let mut target = Saturated {
            full: true,
            received: vec![],
        };
        let mut pending = Pending::new();
        pending.step(&mut source, &mut target).unwrap();
        assert!(!pending.drained());
        pending.step(&mut source, &mut target).unwrap();
        assert!(pending.drained());
        assert_eq!(target.received, bytes);
    }
    fn key(byte: u8) -> String {
        let mut wire = b"\0\0\0\x0bssh-ed25519\0\0\0\x20".to_vec();
        wire.extend_from_slice(&[byte; 32]);
        format!("ssh-ed25519 {}", platform::base64(&wire))
    }
    #[test]
    fn eof_does_not_discard_output_waiting_for_transport_flush() {
        struct Delayed {
            bytes: Vec<u8>,
            emitted: Vec<u8>,
            flushes: usize,
        }
        impl Read for Delayed {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::ErrorKind::WouldBlock.into())
            }
        }
        impl Write for Delayed {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.bytes.extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                self.flushes += 1;
                if self.flushes < 3 {
                    return Err(io::ErrorKind::WouldBlock.into());
                }
                self.emitted.append(&mut self.bytes);
                Ok(())
            }
        }
        let mut source = io::Cursor::new(b"last SSH record".to_vec());
        let mut target = Delayed {
            bytes: vec![],
            emitted: vec![],
            flushes: 0,
        };
        bridge(
            &mut source,
            &mut target,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(target.emitted, b"last SSH record");
        assert!(target.bytes.is_empty());
    }
    fn authorized(peer: &str) -> (Snapshot, Request) {
        let id = random_id().unwrap();
        let mut state = Snapshot::default();
        state.server.running = true;
        state.server.loopback_only = true;
        state.data.incoming.push(Grant {
            id: id.clone(),
            peer: peer.into(),
            label: "test".into(),
            public_key: key(1),
            status: "authorized".into(),
            created: now(),
            account: Some(platform::Account {
                name: "test".into(),
                sid: "S-1-5-21-1".into(),
                admin: false,
            }),
        });
        (
            state,
            Request {
                version: CONTROL_VERSION,
                action: "tunnel".into(),
                id,
                key: key(1),
            },
        )
    }
    #[test]
    fn tunnel_requires_exact_device_grant_and_key_not_an_address() {
        let (mut state, mut request) = authorized("paired-device");
        validate_grant(&state, "paired-device", &request).unwrap();
        assert!(validate_grant(&state, "other-device", &request).is_err());
        request.key = key(2);
        assert!(validate_grant(&state, "paired-device", &request).is_err());
        request.key = key(1);
        for status in ["pending", "granting", "revoked", "cancelled"] {
            state.data.incoming[0].status = status.into();
            assert!(validate_grant(&state, "paired-device", &request).is_err());
        }
        state.data.incoming[0].status = "authorized".into();
        state.server.loopback_only = false;
        assert!(validate_grant(&state, "paired-device", &request).is_err());
        state.server.loopback_only = true;
        request.version = 3;
        assert!(validate_grant(&state, "paired-device", &request).is_err());
    }
    #[test]
    fn fresh_discovery_precedes_old_hints_and_unusable_hints_are_rejected() {
        let fresh = "192.168.1.30:45233".parse().unwrap();
        assert_eq!(
            candidate_address(Some(fresh), Some("192.168.1.20"))
                .unwrap()
                .to_string(),
            "192.168.1.30:45833"
        );
        assert_eq!(
            candidate_address(None, Some("192.168.1.20"))
                .unwrap()
                .to_string(),
            "192.168.1.20:45833"
        );
        for hint in [
            "hostname",
            "0.0.0.0",
            "224.0.0.1",
            "192.168.1.2\nHostName evil",
        ] {
            assert!(candidate_address(None, Some(hint)).is_err());
        }
    }
    #[test]
    fn tunnel_sessions_follow_authenticated_identity_and_individual_cancellation() {
        let shared = Arc::new(Mutex::new(Snapshot::default()));
        let (first, cancel) =
            session(&shared, "grant1", "peer1", "incoming", "192.168.1.2").unwrap();
        let (_second, other) =
            session(&shared, "grant2", "peer2", "incoming", "192.168.1.2").unwrap();
        assert_eq!(shared.lock().unwrap().tunnels[0].session.peer, "peer1");
        cancel.store(true, Ordering::Release);
        assert!(!other.load(Ordering::Acquire));
        drop(first);
        assert_eq!(shared.lock().unwrap().tunnels.len(), 1);
        assert_eq!(shared.lock().unwrap().tunnels[0].session.peer, "peer2");
    }
    #[test]
    fn native_pipe_is_exclusive_local_and_transfers_binary_frames() {
        let name = format!(r"\\.\pipe\Pierdesk-test-{}", random_id().unwrap());
        let mut pipe = create_pipe(&name, true).unwrap();
        assert!(create_pipe(&name, true).is_err());
        let worker = thread::spawn(move || {
            let mut client = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&name)
                .unwrap();
            write_frame(&mut client, &serde_json::json!({"id":"test"})).unwrap();
            let mut bytes = [0_u8; 5];
            client.read_exact(&mut bytes).unwrap();
            assert_eq!(bytes, [0, 255, 1, 128, 3]);
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let result = unsafe { ConnectNamedPipe(pipe.handle(), None) };
            if result.is_ok()
                || result.as_ref().is_err_and(|e| {
                    e.code() == windows_core::HRESULT::from_win32(ERROR_PIPE_CONNECTED.0)
                })
            {
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        let request: LocalRequest = bounded_frame(&mut pipe, &AtomicBool::new(false)).unwrap();
        assert_eq!(request.id, "test");
        pipe.write_all(&[0, 255, 1, 128, 3]).unwrap();
        worker.join().unwrap();
    }
    #[test]
    #[allow(clippy::too_many_lines)] // One real two-ended TLS lifecycle, including slow bulk IO.
    fn pinned_tls_tunnel_transfers_binary_data_and_stops_with_runtime() {
        let root = tempfile::tempdir().unwrap();
        let store = DeviceStore::new(root.path().join("server"));
        let identity = store.load_or_create_identity().unwrap().identity;
        let sender = DeviceStore::new(root.path().join("client"))
            .load_or_create_identity()
            .unwrap()
            .identity;
        store
            .trust_peer(
                sender.certificate_der().to_vec(),
                sender.fingerprint(),
                "sender",
            )
            .unwrap();
        let client = PinnedTlsClient::new(
            &sender,
            identity.certificate_der().to_vec(),
            identity.fingerprint(),
            Duration::from_secs(3),
        )
        .unwrap();
        let tls = TrustedTlsServer::new(identity, store.clone(), Duration::from_secs(3)).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let echo = TcpListener::bind("127.0.0.1:0").unwrap();
        let destination = echo.local_addr().unwrap();
        let echo_worker = thread::spawn(move || {
            let (mut socket, _) = echo.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut b = [0_u8; 8192];
            while let Ok(n) = socket.read(&mut b) {
                if n == 0 {
                    break;
                }
                if socket.write_all(&b[..n]).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
        });
        let (state, request) = authorized(&sender.fingerprint().to_string());
        let shared = Arc::new(Mutex::new(state));
        let stop = Arc::new(AtomicBool::new(false));
        let (server_state, server_stop) = (shared.clone(), stop.clone());
        let worker = thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            let mut connection = tls.accept(socket).unwrap();
            let request: Request = read_frame(&mut connection.stream).unwrap();
            let _ = receive_at(
                &store,
                &server_state,
                connection,
                request,
                &server_stop,
                destination,
            );
        });
        let mut stream = client.connect(address).unwrap();
        write_frame(&mut stream, &request).unwrap();
        let response: Response = read_frame(&mut stream).unwrap();
        assert_eq!(response.status, "tunnel-ready");
        // Interleave unequal blocks over several buffer lengths to exercise partial IO.
        for size in [1, 8191, 65535, 131_071, 524_287] {
            let bytes: Vec<u8> = (0..size).map(|i| u8::try_from(i % 251).unwrap()).collect();
            stream.write_all(&bytes).unwrap();
            stream.flush().unwrap();
            let mut echoed = vec![0; size];
            stream.read_exact(&mut echoed).unwrap();
            assert_eq!(echoed, bytes);
        }
        // Exercise both nonblocking TLS bridges with more data than the OS/TLS send buffers.
        // Concurrent upload/download and a slow receiver reproduce deployment backpressure.
        let local = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut application = TcpStream::connect(local.local_addr().unwrap()).unwrap();
        application
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        application
            .set_write_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let (mut proxy, _) = local.accept().unwrap();
        proxy.set_nonblocking(true).unwrap();
        stream.sock.set_nonblocking(true).unwrap();
        stream.conn.set_buffer_limit(Some(BUFFER_SIZE));
        let client_stop = stop.clone();
        let proxy_worker = thread::spawn(move || {
            bridge(
                &mut proxy,
                &mut stream,
                &client_stop,
                &AtomicBool::new(false),
            )
            .unwrap();
        });
        let bytes: Vec<u8> = (0..16 * 1024 * 1024)
            .map(|i| u8::try_from(i % 251).unwrap())
            .collect();
        let expected = bytes.clone();
        let mut upload = application.try_clone().unwrap();
        let upload_worker = thread::spawn(move || upload.write_all(&bytes).unwrap());
        let mut echoed = vec![0; expected.len()];
        application.read_exact(&mut echoed).unwrap();
        assert_eq!(echoed, expected);
        upload_worker.join().unwrap();
        assert_eq!(shared.lock().unwrap().tunnels.len(), 1);
        stop.store(true, Ordering::Release);
        proxy_worker.join().unwrap();
        worker.join().unwrap();
        assert!(shared.lock().unwrap().tunnels.is_empty());
        echo_worker.join().unwrap();
    }
}
