//! Optional SSH provisioning over the existing paired mutual-TLS identity.
#![allow(clippy::missing_errors_doc, clippy::must_use_candidate)]
mod platform;
mod tunnel;
pub use tunnel::run_proxy;

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::device_store::DeviceStore;
use crate::discovery::DiscoveryView;
use crate::security::{CertificateFingerprint, PinnedTlsClient, TrustedTlsServer};
use serde::{Deserialize, Serialize};

pub use platform::{KEYGEN_SECRET_ENV, run_keygen_askpass};

pub const CONTROL_PORT: u16 = 45833;
const CONTROL_VERSION: u8 = 4;
const DEFAULT_SSH_PORT: u16 = 45834;
const STATE_SCHEMA_VERSION: u8 = 2;
const LIMIT: usize = 32_768;
const TTL: u64 = 600;
const HOST_KEYS_UNAVAILABLE: &str = "SSH host keys unavailable";
const LOGIN_PASSWORD_BYTES: usize = 24;
const LOGIN_PASSWORD_LENGTH: usize = LOGIN_PASSWORD_BYTES * 2;
const CREDENTIAL_LIMIT: usize = 4096;
const CREDENTIAL_MAGIC: &str = "ClipFerry SSH credential v1";

/// Inspect the actual running application gateway without exposing stored credentials.
pub fn access_status() -> io::Result<serde_json::Value> {
    tunnel::runtime_status()
}
/// Explicit, administrator-confirmed setup or migration of this computer's SSH access.
pub fn upgrade_access() -> io::Result<serde_json::Value> {
    platform::setup()?;
    let status = tunnel::runtime_status()?;
    if status["server"]["loopbackOnly"] != true
        || status["server"]["running"] != true
        || status["server"]["firewall"] == "missing"
        || status["listenerError"]
            .as_str()
            .is_some_and(|v| !v.is_empty())
    {
        return Err(invalid(
            "SSH access migration could not be verified; open Remote work and repair local access",
        ));
    }
    Ok(status)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}
fn diagnostic_error(message: &str) -> String {
    let mut result = String::new();
    let mut redact_next = false;
    for word in message.split_whitespace() {
        if !result.is_empty() {
            result.push(' ');
        }
        if redact_next
            || (word.len() > 80
                && word
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"+/=_-".contains(&byte)))
        {
            result.push_str("[redacted]");
            redact_next = false;
        } else {
            result.push_str(word);
            redact_next = matches!(word, "ssh-ed25519" | "ssh-rsa" | "ecdsa-sha2-nistp256");
        }
        if result.len() >= 512 {
            let mut boundary = 509.min(result.len());
            while !result.is_char_boundary(boundary) {
                boundary -= 1;
            }
            result.truncate(boundary);
            result.push_str("...");
            break;
        }
    }
    result
}
fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
}
fn valid_account_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|character| character.is_alphanumeric() || matches!(character, '_' | '.' | '-'))
}
fn approval_can_run(status: &str, created: u64, current: u64) -> bool {
    status == "granting" || (status == "pending" && current.saturating_sub(created) <= TTL)
}
fn approval_matches_started_account(
    status: &str,
    previous: Option<&platform::Account>,
    requested_sid: &str,
) -> bool {
    status != "granting" || previous.is_none_or(|account| account.sid == requested_sid)
}
fn request_is_terminal(status: &str) -> bool {
    matches!(
        status,
        "cancelled" | "expired" | "rejected" | "revoked" | "superseded" | "migration" | "error"
    )
}
fn request_can_be_cancelled(status: &str) -> bool {
    status == "pending" || request_is_terminal(status)
}
fn connection_needs_sync(status: &str, error: &str) -> bool {
    matches!(
        status,
        "pending" | "granting" | "authorized" | "ready" | "offline" | "preparing" | "hostChanged"
    ) || (status == "error" && error.contains(HOST_KEYS_UNAVAILABLE))
}

/// Regular polling skips `error`, which used to freeze a connection (and the `HostName` that
/// external `ssh.exe` reads) forever after one transient failure. Such connections are retried
/// with capped backoff instead.
fn connection_retries_after_error(status: &str, error: &str) -> bool {
    status == "error" && !connection_needs_sync(status, error)
}

const ERROR_RETRY_INITIAL: Duration = Duration::from_secs(30);
const ERROR_RETRY_MAX: Duration = Duration::from_mins(10);

#[derive(Default)]
struct ErrorRetrySchedule(HashMap<String, (Instant, Duration)>);

impl ErrorRetrySchedule {
    /// Returns whether `id` is due for another attempt, advancing its backoff when it is.
    fn take_due(&mut self, id: &str, clock: Instant) -> bool {
        match self.0.get_mut(id) {
            None => {
                self.0.insert(
                    id.to_owned(),
                    (clock + ERROR_RETRY_INITIAL, ERROR_RETRY_INITIAL),
                );
                true
            }
            Some((due, delay)) if clock >= *due => {
                *delay = (*delay * 2).min(ERROR_RETRY_MAX);
                *due = clock + *delay;
                true
            }
            Some(_) => false,
        }
    }

    /// Forgets connections that left the error state, so a later error starts a fresh backoff.
    fn retain(&mut self, still_errored: impl Fn(&str) -> bool) {
        self.0.retain(|id, _| still_errored(id));
    }
}

/// Background sync repeats every few seconds, so only failure changes and recoveries are logged.
fn record_background_sync(
    store: &DeviceStore,
    last_errors: &mut HashMap<String, String>,
    id: &str,
    result: &io::Result<()>,
) {
    match result {
        Err(error) => {
            let message = diagnostic_error(&error.to_string());
            if last_errors.get(id) != Some(&message) {
                append_ssh_diagnostic(
                    store,
                    &format!(
                        "{} background_sync result=failed id={} error={}",
                        now(),
                        id,
                        message
                    ),
                );
                last_errors.insert(id.to_owned(), message);
            }
        }
        Ok(()) => {
            if last_errors.remove(id).is_some() {
                append_ssh_diagnostic(
                    store,
                    &format!("{} background_sync result=recovered id={}", now(), id),
                );
            }
        }
    }
}

fn synchronization_action(initial: bool) -> &'static str {
    if initial { "request" } else { "status" }
}

fn explicit_request_is_initial(status: &str, account: Option<&platform::Account>) -> bool {
    account.is_none() || request_is_terminal(status)
}

fn connection_is_active(status: &str) -> bool {
    matches!(
        status,
        "pending"
            | "granting"
            | "authorized"
            | "ready"
            | "offline"
            | "preparing"
            | "recovery"
            | "conflict"
            | "stale"
            | "hostChanged"
            | "migration"
            | "error"
    )
}

fn canonical_outgoing_index(data: &Data, peer: &str) -> Option<usize> {
    data.outgoing
        .iter()
        .rposition(|connection| connection.peer == peer && connection_is_active(&connection.status))
        .or_else(|| {
            data.outgoing
                .iter()
                .rposition(|connection| connection.peer == peer)
        })
}
fn random_id() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| invalid(e.to_string()))?;
    let mut id = String::with_capacity(32);
    for b in bytes {
        let _ = write!(id, "{b:02x}");
    }
    Ok(id)
}

fn generate_login_password() -> io::Result<String> {
    let mut bytes = [0_u8; LOGIN_PASSWORD_BYTES];
    getrandom::fill(&mut bytes).map_err(|error| invalid(error.to_string()))?;
    let mut password = String::with_capacity(LOGIN_PASSWORD_LENGTH);
    for byte in bytes {
        let _ = write!(password, "{byte:02x}");
    }
    Ok(password)
}

fn valid_login_password(password: &str) -> bool {
    password.len() == LOGIN_PASSWORD_LENGTH
        && password
            .bytes()
            .all(|byte| b"0123456789abcdef".contains(&byte))
}

fn credential_path(store: &DeviceStore, kind: &str, id: &str) -> io::Result<PathBuf> {
    if !valid_id(id) || !matches!(kind, "client" | "server") {
        return Err(invalid("Invalid SSH credential reference"));
    }
    Ok(store
        .root()
        .join("ssh/credentials")
        .join(format!("{kind}-{id}.dpapi")))
}

fn pending_client_credential_path(store: &DeviceStore, id: &str) -> io::Result<PathBuf> {
    Ok(credential_path(store, "client", id)?.with_extension("pending.dpapi"))
}

fn read_login_password_file(path: &Path, kind: &str, id: &str) -> io::Result<String> {
    let protected = fs::read(path)?;
    if protected.is_empty() || protected.len() > CREDENTIAL_LIMIT {
        return Err(invalid("Invalid protected SSH credential"));
    }
    let cleartext = crate::device_store::unprotect_for_current_user(&protected)?;
    let text = std::str::from_utf8(&cleartext).map_err(io::Error::other)?;
    let prefix = format!("{CREDENTIAL_MAGIC}\n{kind}\n{id}\n");
    let password = text
        .strip_prefix(&prefix)
        .ok_or_else(|| invalid("SSH credential belongs to another authorization"))?;
    if !valid_login_password(password) {
        return Err(invalid("Invalid SSH login password"));
    }
    Ok(password.to_ascii_lowercase())
}

fn read_login_password(store: &DeviceStore, kind: &str, id: &str) -> io::Result<String> {
    read_login_password_file(&credential_path(store, kind, id)?, kind, id)
}

fn optional_login_password_file(path: &Path, kind: &str, id: &str) -> io::Result<Option<String>> {
    match read_login_password_file(path, kind, id) {
        Ok(password) => Ok(Some(password)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn optional_login_password(
    store: &DeviceStore,
    kind: &str,
    id: &str,
) -> io::Result<Option<String>> {
    match read_login_password(store, kind, id) {
        Ok(password) => Ok(Some(password)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn write_login_password(
    store: &DeviceStore,
    kind: &str,
    id: &str,
    password: &str,
) -> io::Result<()> {
    let path = credential_path(store, kind, id)?;
    let temporary = path.with_extension("next");
    write_login_password_file(&path, &temporary, kind, id, password)
}

fn write_login_password_file(
    path: &Path,
    temporary: &Path,
    kind: &str,
    id: &str,
    password: &str,
) -> io::Result<()> {
    if !valid_login_password(password) {
        return Err(invalid("Invalid SSH login password"));
    }
    let directory = path
        .parent()
        .ok_or_else(|| invalid("SSH credential path has no parent"))?;
    fs::create_dir_all(directory)?;
    let cleartext = format!("{CREDENTIAL_MAGIC}\n{kind}\n{id}\n{password}");
    let protected = crate::device_store::protect_for_current_user(cleartext.as_bytes())?;
    write_atomic(path, temporary, &protected)
}

fn remove_login_password(store: &DeviceStore, kind: &str, id: &str) -> io::Result<()> {
    match fs::remove_file(credential_path(store, kind, id)?) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn load_or_create_server_password(store: &DeviceStore, id: &str) -> io::Result<String> {
    if let Some(password) = optional_login_password(store, "server", id)? {
        Ok(password)
    } else {
        let password = generate_login_password()?;
        write_login_password(store, "server", id, &password)?;
        Ok(password)
    }
}

fn matching_client_passphrase(
    store: &DeviceStore,
    id: &str,
    expected_public_key: &str,
    askpass: &Path,
) -> io::Result<String> {
    let key_path = store.root().join("ssh").join(id);
    let primary_path = credential_path(store, "client", id)?;
    let pending_path = pending_client_credential_path(store, id)?;
    let primary = optional_login_password_file(&primary_path, "client", id)?;
    let pending = optional_login_password_file(&pending_path, "client", id)?;

    let mut candidates = Vec::new();
    if let Some(candidate) = primary.as_ref() {
        candidates.push(candidate.as_str());
    }
    if let Some(candidate) = pending.as_ref()
        && !candidates.contains(&candidate.as_str())
    {
        candidates.push(candidate.as_str());
    }
    if !candidates.contains(&"") {
        candidates.push("");
    }
    let mut tool_failure = None;
    for candidate in candidates {
        match platform::public_key(&key_path, candidate, askpass)
            .and_then(|key| platform::validate_public_key(&key))
        {
            Ok(key) if key == expected_public_key => return Ok(candidate.to_owned()),
            Ok(_) => {}
            Err(error) if platform::is_passphrase_mismatch(&error) => {}
            Err(error) => {
                tool_failure.get_or_insert(error);
            }
        }
    }
    // A key that cannot be checked is not evidence of a bad credential (CF-SSH-002: ssh-keygen
    // could not start while Windows was out of virtual memory).
    Err(match tool_failure {
        Some(error) => invalid(format!(
            "Cannot run ssh-keygen to verify the local SSH key: {error}"
        )),
        None => invalid("The local SSH private key cannot be unlocked with its saved credential"),
    })
}

fn install_client_password(
    store: &DeviceStore,
    id: &str,
    password: &str,
    expected_public_key: &str,
    askpass: &Path,
) -> io::Result<()> {
    if !valid_login_password(password) {
        return Err(invalid("The target returned an invalid SSH login password"));
    }
    let key_path = store.root().join("ssh").join(id);
    let pending_path = pending_client_credential_path(store, id)?;
    let actual = matching_client_passphrase(store, id, expected_public_key, askpass)?;

    if actual != password {
        let pending_temp = pending_path.with_extension("pending.next");
        write_login_password_file(&pending_path, &pending_temp, "client", id, password)?;
        platform::set_key_passphrase(&key_path, &actual, password, askpass)?;
    }
    write_login_password(store, "client", id, password)?;
    match fs::remove_file(pending_path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(())
}

/// Prints the current authorization's DPAPI-protected private-key passphrase for OpenSSH's
/// `SSH_ASKPASS` callback. This mode is entered only when `Pierdesk` itself sets the environment.
pub fn run_askpass() -> io::Result<()> {
    let id = std::env::var("PIERDESK_SSH_ASKPASS_ID")
        .map_err(|_| invalid("Missing SSH askpass authorization"))?;
    if !valid_id(&id) {
        return Err(invalid("Invalid SSH askpass authorization"));
    }
    let store = DeviceStore::current_user()?;
    let password = read_login_password(&store, "client", &id)?;
    print!("{password}");
    io::stdout().flush()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Grant {
    pub id: String,
    pub peer: String,
    pub label: String,
    pub public_key: String,
    pub status: String,
    pub created: u64,
    pub account: Option<platform::Account>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Connection {
    pub id: String,
    pub peer: String,
    pub status: String,
    pub public_key: String,
    pub host_keys: Vec<String>,
    pub offered_keys: Vec<String>,
    pub address: String,
    pub port: u16,
    pub account: Option<platform::Account>,
    pub tested: u64,
    pub admin_verified: bool,
    pub error: String,
    pub created: u64,
    pub command: String,
    #[serde(default)]
    pub host_fingerprints: Vec<String>,
    #[serde(default)]
    pub offered_fingerprints: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveSession {
    pub id: String,
    pub connection_id: String,
    pub peer: String,
    pub direction: String,
    pub address: String,
    pub port: u16,
    pub started: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Data {
    #[serde(default)]
    schema: u8,
    incoming: Vec<Grant>,
    outgoing: Vec<Connection>,
}

impl Default for Data {
    fn default() -> Self {
        Self {
            schema: STATE_SCHEMA_VERSION,
            incoming: Vec::new(),
            outgoing: Vec::new(),
        }
    }
}

fn migrate_to_dedicated_runtime(data: &mut Data, timestamp: u64) -> bool {
    if data.schema == STATE_SCHEMA_VERSION {
        return false;
    }

    // Old builds used Windows' sshd, port 22 and a wire protocol with different authorization
    // semantics. Keep reusable private keys, but invalidate every derived connection detail and
    // require an explicit request through the dedicated service.
    let mut incoming_peers = std::collections::HashSet::new();
    for grant in data.incoming.iter_mut().rev() {
        if matches!(grant.status.as_str(), "pending" | "granting" | "authorized") {
            grant.account = None;
            if incoming_peers.insert(grant.peer.clone()) {
                // Do not notify the receiver until the updated requester explicitly submits the
                // v2 request. The matching request id/key will turn this migration marker into a
                // normal pending approval in `respond`.
                grant.status = "migration".into();
                grant.created = timestamp;
            } else {
                grant.status = "superseded".into();
            }
        }
    }

    let mut outgoing_peers = std::collections::HashSet::new();
    for connection in data.outgoing.iter_mut().rev() {
        let was_active = connection_is_active(&connection.status);
        connection.port = DEFAULT_SSH_PORT;
        connection.account = None;
        connection.tested = 0;
        connection.admin_verified = false;
        connection.command.clear();
        connection.host_keys.clear();
        connection.offered_keys.clear();
        connection.host_fingerprints.clear();
        connection.offered_fingerprints.clear();
        if was_active && outgoing_peers.insert(connection.peer.clone()) {
            connection.status = "migration".into();
            connection.error =
                "SSH access needs reapproval after the Pierdesk dedicated SSH upgrade".into();
        } else if was_active {
            connection.status = "superseded".into();
            connection.error.clear();
        }
    }
    data.schema = STATE_SCHEMA_VERSION;
    true
}

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    data: Data,
    server: platform::ServerInfo,
    sessions: Vec<ActiveSession>,
    login_passwords: HashMap<String, String>,
    busy: bool,
    operation: String,
    error: String,
    listener_error: String,
    local_proxy_error: String,
    remote_access: HashMap<String, AccessInfo>,
    #[serde(skip)]
    tunnels: Vec<tunnel::LiveTunnel>,
    events: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    Refresh,
    Setup,
    SetupClient,
    Request(String),
    Approve { id: String, sid: String },
    Reject(String),
    Cancel(String),
    Revoke(String),
    Test(String),
    TrustHost { id: String, expected: String },
    Terminal(String),
    Disconnect(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u8,
    action: String,
    id: String,
    key: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AccessInfo {
    transport: String,
    state: String,
    local_address: String,
    network: Option<serde_json::Value>,
}
fn access_info(server: &platform::ServerInfo) -> AccessInfo {
    AccessInfo {
        transport: "tunnel-v4".into(),
        state: if !server.running {
            "service-stopped"
        } else if !server.loopback_only {
            "upgrade-required"
        } else if server.firewall == "missing" {
            "rules-missing"
        } else {
            "ready"
        }
        .into(),
        local_address: String::new(),
        network: None,
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    version: u8,
    status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    access: Option<AccessInfo>,
    account: Option<platform::Account>,
    port: u16,
    keys: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    login_password: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    error: String,
}

impl std::fmt::Debug for Response {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Response")
            .field("version", &self.version)
            .field("status", &self.status)
            .field("access", &self.access)
            .field("account", &self.account)
            .field("port", &self.port)
            .field("keys", &self.keys)
            .field(
                "login_password",
                &if self.login_password.is_empty() {
                    ""
                } else {
                    "[redacted]"
                },
            )
            .field("error", &self.error)
            .finish()
    }
}

#[derive(Debug)]
struct RemoteControlError {
    code: String,
}

impl std::fmt::Display for RemoteControlError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(control_error_message(&self.code))
    }
}

impl std::error::Error for RemoteControlError {}

fn remote_control_error_code(error: &io::Error) -> Option<&str> {
    error
        .get_ref()?
        .downcast_ref::<RemoteControlError>()
        .map(|remote| remote.code.as_str())
}

fn peer_key_grant(data: &Data, peer: &str, key: &str) -> Option<usize> {
    data.incoming
        .iter()
        .enumerate()
        .filter(|(_, grant)| grant.peer == peer && grant.public_key == key)
        .max_by_key(|(_, grant)| (!request_is_terminal(&grant.status), grant.created))
        .map(|(index, _)| index)
}

fn append_ssh_diagnostic(store: &DeviceStore, message: &str) {
    let _ = crate::logging::append(&store.root().join("logs/ssh.log"), message);
    let _ = crate::logging::append(
        &store.root().join("logs/clipferry.log"),
        &format!("ssh {message}"),
    );
}

fn control_error_code(error: &io::Error) -> &'static str {
    let message = error.to_string();
    if message.contains("authorization recovery is required") {
        "authorization-recovery-required"
    } else if message.contains("authorization already exists") {
        "existing-authorization"
    } else if message.contains("Authorization request not found") {
        "request-not-found"
    } else if message.contains("Authorization key changed") {
        "key-changed"
    } else if message.contains("Unsupported SSH control request") {
        "version-mismatch"
    } else if message.contains("Invalid Ed25519") {
        "invalid-key"
    } else {
        "request-failed"
    }
}

fn control_error_message(code: &str) -> &'static str {
    match code {
        "authorization-recovery-required" => {
            "对方电脑有一项未完成的管理员授权，请在对方电脑完成授权或撤销该记录"
        }
        "existing-authorization" => {
            "对方电脑已有这台设备的待处理或已授权记录，请先在对方电脑撤销旧记录"
        }
        "request-not-found" => "对方电脑已找不到这项请求，请取消后重新申请",
        "key-changed" => "请求使用的 SSH 密钥发生变化，请取消后重新申请",
        "version-mismatch" => "两台电脑的 Pierdesk SSH 协议版本不一致",
        "invalid-key" => "对方电脑拒绝了无效的 SSH 公钥",
        _ => "对方电脑无法处理这项 SSH 请求，请在对方电脑查看远程工作状态",
    }
}

pub struct Runtime {
    shared: Arc<Mutex<Snapshot>>,
    store: DeviceStore,
    sender: mpsc::SyncSender<Command>,
    stop: Arc<AtomicBool>,
    listener: Option<JoinHandle<()>>,
    local_proxy: Option<JoinHandle<()>>,
    worker: Option<JoinHandle<()>>,
}

impl Runtime {
    /// Starts independent network and provisioning workers.
    ///
    /// # Panics
    /// A worker can panic if another thread poisons the internal state lock.
    #[allow(clippy::too_many_lines)] // Worker lifecycle and shutdown ownership stay together.
    pub fn start(store: DeviceStore, discovery: Option<DiscoveryView>) -> io::Result<Self> {
        crate::license_runtime::require_access()?;
        let root = store.root().join("ssh");
        fs::create_dir_all(&root)?;
        let (mut data, recovery_warning) = load_persisted_state(&store);
        if migrate_to_dedicated_runtime(&mut data, now()) {
            for connection in &data.outgoing {
                let _ = fs::remove_file(root.join(format!("{}.config", connection.id)));
                let _ = fs::remove_file(root.join(format!("{}.known_hosts", connection.id)));
            }
            persist(&store, &data)?;
            append_ssh_diagnostic(
                &store,
                &format!(
                    "{} state_migration schema={} outgoing_requires_reapproval",
                    now(),
                    STATE_SCHEMA_VERSION
                ),
            );
        }
        for c in &mut data.outgoing {
            c.tested = 0;
            c.admin_verified = false;
            c.command.clear();
            if c.status == "ready" {
                c.status = "authorized".into();
            }
        }
        let shared = Arc::new(Mutex::new(Snapshot {
            data,
            error: recovery_warning.unwrap_or_default(),
            ..Snapshot::default()
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let server_inspection_complete = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::sync_channel(16);
        let server_store = store.clone();
        let server_shared = shared.clone();
        let server_stop = stop.clone();
        let listener_inspection_complete = server_inspection_complete.clone();
        let listener = thread::Builder::new()
            .name("ssh-control".into())
            .spawn(move || {
                // Avoid answering from the default, empty ServerInfo while startup inspection is
                // still loading the dedicated SSH host identity. Peers already retry control
                // connections, whereas an incomplete authorized response used to persist as an
                // unrecoverable host-key error.
                while !listener_inspection_complete.load(Ordering::Acquire)
                    && !server_stop.load(Ordering::Acquire)
                {
                    thread::sleep(Duration::from_millis(25));
                }
                if server_stop.load(Ordering::Acquire) {
                    return;
                }
                if let Err(error) = serve(&server_store, &server_shared, &server_stop)
                    && let Ok(mut state) = server_shared.lock()
                {
                    state.listener_error = error.to_string();
                }
            })?;
        let worker_shared = shared.clone();
        let worker_stop = stop.clone();
        let worker_inspection_complete = server_inspection_complete;
        let local_store = store.clone();
        let local_discovery = discovery.clone();
        let local_shared = shared.clone();
        let local_stop = stop.clone();
        let local_proxy = thread::Builder::new()
            .name("ssh-local-proxy".into())
            .spawn(move || {
                if let Err(error) = tunnel::serve_local(
                    local_store,
                    local_discovery,
                    local_shared.clone(),
                    local_stop,
                ) {
                    local_shared.lock().unwrap().local_proxy_error = error.to_string();
                }
            })?;
        let runtime_store = store.clone();
        let worker = thread::Builder::new()
            .name("ssh-operations".into())
            .spawn(move || {
                match platform::inspect(&store.root().join("ssh")) {
                    Ok(info) => worker_shared.lock().unwrap().server = info,
                    Err(error) => {
                        worker_shared.lock().unwrap().error =
                            format!("Cannot inspect Windows SSH: {error}");
                    }
                }
                worker_inspection_complete.store(true, Ordering::Release);
                let mut ticks = 0u8;
                let mut last_inspection = Instant::now();
                let mut error_retries = ErrorRetrySchedule::default();
                let mut sync_errors = HashMap::new();
                while !worker_stop.load(Ordering::Acquire) {
                    match receiver.recv_timeout(Duration::from_secs(1)) {
                        Ok(command) => {
                            {
                                let mut state = worker_shared.lock().unwrap();
                                state.busy = true;
                                state.error.clear();
                                state.operation = command_name(&command).into();
                            }
                            let context = command_context(&command);
                            let result =
                                execute(&store, discovery.as_ref(), &worker_shared, command);
                            let mut state = worker_shared.lock().unwrap();
                            state.busy = false;
                            if let Err(e) = result {
                                state.error = e.to_string();
                            }
                            let event = if state.error.is_empty() {
                                format!(
                                    "{} operation={} result=completed{}",
                                    now(),
                                    state.operation,
                                    context
                                )
                            } else {
                                format!(
                                    "{} operation={} result=failed{} error={}",
                                    now(),
                                    state.operation,
                                    context,
                                    diagnostic_error(&state.error)
                                )
                            };
                            state.events.push(event.clone());
                            if state.events.len() > 20 {
                                state.events.remove(0);
                            }
                            append_ssh_diagnostic(&store, &event);
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    if last_inspection.elapsed() >= Duration::from_secs(30) {
                        if let Ok(info) = platform::inspect(&store.root().join("ssh")) {
                            worker_shared.lock().unwrap().server = info;
                        }
                        last_inspection = Instant::now();
                    }
                    ticks = ticks.wrapping_add(1);
                    if ticks >= 5 {
                        ticks = 0;
                        let ids = {
                            let state = worker_shared.lock().unwrap();
                            let outgoing = &state.data.outgoing;
                            error_retries.retain(|id| {
                                outgoing.iter().any(|c| {
                                    c.id == id
                                        && connection_retries_after_error(&c.status, &c.error)
                                })
                            });
                            let clock = Instant::now();
                            outgoing
                                .iter()
                                .filter(|c| {
                                    connection_needs_sync(&c.status, &c.error)
                                        || (connection_retries_after_error(&c.status, &c.error)
                                            && error_retries.take_due(&c.id, clock))
                                })
                                .map(|c| c.id.clone())
                                .collect::<Vec<_>>()
                        };
                        for id in ids {
                            if worker_stop.load(Ordering::Acquire) {
                                break;
                            }
                            let result = sync_connection(
                                &store,
                                discovery.as_ref(),
                                &worker_shared,
                                &id,
                                false,
                            );
                            record_background_sync(&store, &mut sync_errors, &id, &result);
                            let needs_test =
                                worker_shared.lock().unwrap().data.outgoing.iter().any(|c| {
                                    c.id == id
                                        && c.status == "authorized"
                                        && c.tested == 0
                                        && c.error.is_empty()
                                });
                            if needs_test {
                                {
                                    let mut state = worker_shared.lock().unwrap();
                                    state.busy = true;
                                    state.operation = "test".into();
                                }
                                let _ = perform_connection(
                                    &store,
                                    discovery.as_ref(),
                                    &worker_shared,
                                    &id,
                                    false,
                                );
                                worker_shared.lock().unwrap().busy = false;
                            }
                        }
                        let mut state = worker_shared.lock().unwrap();
                        for grant in &mut state.data.incoming {
                            if grant.status == "pending"
                                && now().saturating_sub(grant.created) > TTL
                            {
                                grant.status = "expired".into();
                            }
                        }
                        let _ = persist(&store, &state.data);
                    }
                }
            })?;
        Ok(Self {
            shared,
            store: runtime_store,
            sender,
            stop,
            listener: Some(listener),
            local_proxy: Some(local_proxy),
            worker: Some(worker),
        })
    }
    pub fn snapshot(&self) -> Snapshot {
        let mut state = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.sessions = state.tunnels.iter().map(|t| t.session.clone()).collect();
        let mut snapshot = state.clone();
        snapshot.login_passwords = snapshot
            .data
            .outgoing
            .iter()
            .filter(|connection| {
                matches!(
                    connection.status.as_str(),
                    "authorized" | "ready" | "offline"
                )
            })
            .filter_map(|connection| {
                optional_login_password(&self.store, "client", &connection.id)
                    .ok()
                    .flatten()
                    .map(|password| (connection.id.clone(), password))
            })
            .collect();
        snapshot
    }
    pub fn active_session_count(&self) -> usize {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tunnels
            .len()
    }
    pub fn pending_incoming_requests(&self) -> Vec<(String, String)> {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .data
            .incoming
            .iter()
            .filter(|grant| grant.status == "pending")
            .map(|grant| (grant.id.clone(), grant.label.clone()))
            .collect()
    }
    pub fn command(&self, command: Command) -> io::Result<()> {
        let operation = command_name(&command);
        {
            let mut state = self
                .shared
                .lock()
                .map_err(|_| invalid("SSH worker state unavailable"))?;
            if state.busy {
                return Err(invalid("An SSH operation is already running"));
            }
            // Reserve the single-operation worker before enqueueing. Without this reservation two
            // rapid UI clicks can both observe `busy == false` and queue duplicate UAC/cancel
            // operations before the worker publishes its next snapshot.
            state.busy = true;
            state.operation = operation.into();
        }
        if self.sender.try_send(command).is_err() {
            self.shared
                .lock()
                .map_err(|_| invalid("SSH worker state unavailable"))?
                .busy = false;
            return Err(invalid("SSH operation queue is full"));
        }
        Ok(())
    }
    pub fn ensure_can_unpair(&self, fingerprint: &str) -> io::Result<()> {
        if self
            .shared
            .lock()
            .map_err(|_| invalid("SSH worker state unavailable"))?
            .data
            .incoming
            .iter()
            .any(|g| {
                g.peer == fingerprint && matches!(g.status.as_str(), "authorized" | "granting")
            })
        {
            Err(invalid(
                "Revoke this device's SSH authorization in Remote work before unpairing.",
            ))
        } else {
            Ok(())
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // UAC/install may still be running. Do not block tray shutdown on an interactive prompt.
        self.worker.take();
        if let Some(handle) = self.local_proxy.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.listener.take() {
            let _ = handle.join();
        }
    }
}

fn command_name(command: &Command) -> &'static str {
    match command {
        Command::Setup => "setup",
        Command::SetupClient => "setup-client",
        Command::Approve { .. } => "approve",
        Command::Revoke(_) => "revoke",
        Command::Test(_) => "test",
        Command::Request(_) => "request",
        Command::Reject(_) => "reject",
        Command::Cancel(_) => "cancel",
        Command::TrustHost { .. } => "trust-host",
        Command::Terminal(_) => "terminal",
        Command::Disconnect(_) => "disconnect",
        Command::Refresh => "refresh",
    }
}

fn command_context(command: &Command) -> String {
    match command {
        Command::Request(peer) => format!(" peer={peer}"),
        Command::Approve { id, sid } => format!(" id={id} account_sid={sid}"),
        Command::Revoke(id)
        | Command::Test(id)
        | Command::Reject(id)
        | Command::Cancel(id)
        | Command::Terminal(id)
        | Command::Disconnect(id)
        | Command::TrustHost { id, .. } => format!(" id={id}"),
        Command::Setup | Command::SetupClient | Command::Refresh => String::new(),
    }
}

fn persist(store: &DeviceStore, data: &Data) -> io::Result<()> {
    let root = store.root().join("ssh");
    let path = root.join("state.json");
    let backup = root.join("state.backup.json");
    let bytes = serde_json::to_vec_pretty(data).map_err(io::Error::other)?;

    // Keep the last valid primary before publishing the next state. Never replace a usable backup
    // with a corrupt/partial primary left by an interrupted process.
    if let Ok(previous) = read_state_file(&path) {
        let previous = serde_json::to_vec_pretty(&previous).map_err(io::Error::other)?;
        write_atomic(&backup, &root.join("state.backup.next"), &previous)?;
    }
    write_atomic(&path, &root.join("state.next"), &bytes)?;
    if read_state_file(&backup).is_err() {
        write_atomic(&backup, &root.join("state.backup.next"), &bytes)?;
    }
    Ok(())
}

fn read_state_file(path: &Path) -> io::Result<Data> {
    let bytes = fs::read(path)?;
    if bytes.len() > 1_048_576 {
        return Err(invalid("SSH state exceeds size limit"));
    }
    let data: Data = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if data.schema > STATE_SCHEMA_VERSION
        || data.incoming.len() > 128
        || data.outgoing.len() > 128
        || data.incoming.iter().any(|grant| !valid_id(&grant.id))
        || data
            .outgoing
            .iter()
            .any(|connection| !valid_id(&connection.id))
    {
        return Err(invalid("Invalid saved SSH authorization records"));
    }
    Ok(data)
}

fn load_persisted_state(store: &DeviceStore) -> (Data, Option<String>) {
    let root = store.root().join("ssh");
    let primary = root.join("state.json");
    let backup = root.join("state.backup.json");
    match read_state_file(&primary) {
        Ok(data) => (data, None),
        Err(primary_error) => match read_state_file(&backup) {
            Ok(data) => {
                let warning = format!(
                    "SSH state was restored from its backup because state.json was unavailable: {}",
                    diagnostic_error(&primary_error.to_string())
                );
                append_ssh_diagnostic(
                    store,
                    &format!(
                        "{} state_recovery source=backup primary_error={}",
                        now(),
                        diagnostic_error(&primary_error.to_string())
                    ),
                );
                if let Err(error) = persist(store, &data) {
                    append_ssh_diagnostic(
                        store,
                        &format!(
                            "{} state_recovery_restore_failed error={}",
                            now(),
                            diagnostic_error(&error.to_string())
                        ),
                    );
                }
                (data, Some(warning))
            }
            Err(backup_error)
                if primary_error.kind() == io::ErrorKind::NotFound
                    && backup_error.kind() == io::ErrorKind::NotFound =>
            {
                (Data::default(), None)
            }
            Err(backup_error) => {
                let warning = "SSH authorization state is damaged and no usable backup is available. Existing SSH keys were not changed; review local SSH access before authorizing again.".to_string();
                append_ssh_diagnostic(
                    store,
                    &format!(
                        "{} state_recovery source=empty primary_error={} backup_error={}",
                        now(),
                        diagnostic_error(&primary_error.to_string()),
                        diagnostic_error(&backup_error.to_string())
                    ),
                );
                (Data::default(), Some(warning))
            }
        },
    }
}

fn write_atomic(path: &Path, temp: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    replace_file(temp, path)
}

fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    let a = from
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let b = to
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    unsafe {
        MoveFileExW(
            windows_core::PCWSTR(a.as_ptr()),
            windows_core::PCWSTR(b.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(io::Error::other)
}

fn read_frame<T: for<'de> Deserialize<'de>>(stream: &mut impl Read) -> io::Result<T> {
    // Socket timeouts alone can be extended indefinitely by a byte-at-a-time sender.
    struct Timed<'a, R>(&'a mut R, std::time::Instant);
    impl<R: Read> Read for Timed<'_, R> {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            if self.1.elapsed() > Duration::from_secs(5) {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "SSH control frame deadline exceeded",
                ));
            }
            self.0.read(bytes)
        }
    }
    let stream = &mut Timed(stream, std::time::Instant::now());
    read_frame_payload(stream)
}
fn read_frame_payload<T: for<'de> Deserialize<'de>>(stream: &mut impl Read) -> io::Result<T> {
    let mut size = [0u8; 4];
    stream.read_exact(&mut size)?;
    let size = u32::from_be_bytes(size) as usize;
    if size == 0 || size > LIMIT {
        return Err(invalid("Invalid SSH control frame size"));
    }
    let mut bytes = vec![0; size];
    stream.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}
fn write_frame<T: Serialize>(stream: &mut impl Write, value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    if bytes.len() > LIMIT {
        return Err(invalid("SSH control frame too large"));
    }
    let size = u32::try_from(bytes.len()).map_err(io::Error::other)?;
    stream.write_all(&size.to_be_bytes())?;
    stream.write_all(&bytes)?;
    stream.flush()
}

fn serve(
    store: &DeviceStore,
    shared: &Arc<Mutex<Snapshot>>,
    stop: &Arc<AtomicBool>,
) -> io::Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", CONTROL_PORT))?;
    listener.set_nonblocking(true)?;
    let tls = TrustedTlsServer::new(
        store.load_identity()?,
        store.clone(),
        Duration::from_secs(2),
    )?;
    let mut workers: Vec<JoinHandle<()>> = vec![];
    while !stop.load(Ordering::Acquire) {
        workers.retain(|w| !w.is_finished());
        match listener.accept() {
            Ok((socket, _)) if workers.len() < 32 => {
                let (store, shared, tls, stop) =
                    (store.clone(), shared.clone(), tls.clone(), stop.clone());
                workers.push(thread::spawn(move || {
                    let _ = serve_connection(&store, &shared, &tls, socket, &stop);
                }));
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(15));
            }
            Err(e) => return Err(e),
        }
    }
    for worker in workers {
        let _ = worker.join();
    }
    Ok(())
}

fn serve_connection(
    store: &DeviceStore,
    shared: &Arc<Mutex<Snapshot>>,
    tls: &TrustedTlsServer,
    socket: std::net::TcpStream,
    stop: &AtomicBool,
) -> io::Result<()> {
    let mut connection = tls.accept_cancellable(socket, stop)?;
    let mut response = match read_frame::<Request>(&mut connection.stream) {
        Ok(request) if request.action == "tunnel" => {
            return tunnel::receive(store, shared, connection, request, stop);
        }
        Ok(request) => respond(store, shared, connection.peer_fingerprint, &request)
            .unwrap_or_else(|error| {
                let code = control_error_code(&error);
                append_ssh_diagnostic(
                    store,
                    &format!(
                        "{} control_request peer={} action={} id={} result={} details={}",
                        now(),
                        connection.peer_fingerprint,
                        request.action,
                        request.id,
                        code,
                        diagnostic_error(&error.to_string())
                    ),
                );
                Response {
                    version: CONTROL_VERSION,
                    access: None,
                    status: "error".into(),
                    account: None,
                    port: 0,
                    keys: vec![],
                    login_password: String::new(),
                    error: code.into(),
                }
            }),
        Err(error) => {
            append_ssh_diagnostic(
                store,
                &format!(
                    "{} control_request peer={} result=request-failed details={}",
                    now(),
                    connection.peer_fingerprint,
                    diagnostic_error(&error.to_string())
                ),
            );
            Response {
                version: CONTROL_VERSION,
                access: None,
                status: "error".into(),
                account: None,
                port: 0,
                keys: vec![],
                login_password: String::new(),
                error: "request-failed".into(),
            }
        }
    };
    if let Some(access) = response.access.as_mut() {
        let local = connection.stream.sock.local_addr()?.ip().to_string();
        access.local_address.clone_from(&local);
        access.network = shared
            .lock()
            .unwrap()
            .server
            .networks
            .iter()
            .find(|network| {
                network["addresses"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(&local)))
            })
            .cloned();
    }
    write_frame(&mut connection.stream, &response)?;
    connection.stream.conn.send_close_notify();
    connection.stream.flush()
}

#[allow(clippy::too_many_lines)] // Validation and all three wire actions share one locked state transition.
fn respond(
    store: &DeviceStore,
    shared: &Mutex<Snapshot>,
    peer: CertificateFingerprint,
    request: &Request,
) -> io::Result<Response> {
    if request.version != CONTROL_VERSION || !valid_id(&request.id) {
        return Err(invalid("Unsupported SSH control request"));
    }
    if !matches!(request.action.as_str(), "request" | "status" | "cancel") {
        return Err(invalid("Unsupported action"));
    }
    let key = platform::validate_public_key(&request.key)?;
    let trusted = store.load_peer(peer)?;
    let peer = peer.to_string();
    let current = now();
    let mut state = shared.lock().unwrap();

    // Expiration is evaluated before conflict detection. Otherwise a request that has already
    // expired can block every later request until the background maintenance tick happens to run.
    for grant in &mut state.data.incoming {
        if grant.peer == peer
            && grant.status == "pending"
            && current.saturating_sub(grant.created) > TTL
        {
            grant.status = "expired".into();
        }
    }

    let exact = state
        .data
        .incoming
        .iter()
        .position(|grant| grant.peer == peer && grant.id == request.id);
    let index = match request.action.as_str() {
        "request" => {
            if let Some(index) = exact {
                if state.data.incoming[index].public_key != key {
                    return Err(invalid("Authorization key changed"));
                }
                if request_is_terminal(&state.data.incoming[index].status) {
                    if state
                        .data
                        .incoming
                        .iter()
                        .enumerate()
                        .any(|(other, grant)| {
                            other != index
                                && grant.peer == peer
                                && matches!(grant.status.as_str(), "granting" | "authorized")
                        })
                    {
                        persist(store, &state.data)?;
                        return Err(invalid(
                            "An authorization recovery is required for this device",
                        ));
                    }
                    for (other, grant) in state.data.incoming.iter_mut().enumerate() {
                        if other != index && grant.peer == peer && grant.status == "pending" {
                            grant.status = "superseded".into();
                        }
                    }
                    let grant = &mut state.data.incoming[index];
                    grant.status = "pending".into();
                    grant.created = current;
                    grant.account = None;
                    grant.label.clone_from(&trusted.label);
                    remove_login_password(store, "server", &grant.id)?;
                }
                index
            } else if let Some(index) = peer_key_grant(&state.data, &peer, &key)
                .filter(|index| !request_is_terminal(&state.data.incoming[*index].status))
            {
                // The requester can lose state.json while its private key survives. In that case
                // it may present the same key under a reconstructed local record id. Associate the
                // request by the mutually authenticated device and key instead of creating a
                // duplicate authorization that can never converge.
                index
            } else {
                if state
                    .data
                    .incoming
                    .iter()
                    .any(|grant| grant.peer == peer && grant.status == "granting")
                {
                    persist(store, &state.data)?;
                    return Err(invalid(
                        "An authorization recovery is required for this device",
                    ));
                }
                if state
                    .data
                    .incoming
                    .iter()
                    .any(|grant| grant.peer == peer && grant.status == "authorized")
                {
                    persist(store, &state.data)?;
                    return Err(invalid("An authorization already exists for this device"));
                }

                // A peer may have lost its local pending record after a crash or an older buggy
                // cancellation. A new, still-unapproved request safely supersedes that old record.
                for grant in &mut state.data.incoming {
                    if grant.peer == peer && grant.status == "pending" {
                        grant.status = "superseded".into();
                    }
                }
                if state.data.incoming.len() >= 128 {
                    state.data.incoming.retain(|grant| {
                        matches!(grant.status.as_str(), "pending" | "granting" | "authorized")
                    });
                }
                if state.data.incoming.len() >= 128 {
                    return Err(invalid("Too many authorization records"));
                }
                state.data.incoming.push(Grant {
                    id: request.id.clone(),
                    peer: peer.clone(),
                    label: trusted.label,
                    public_key: key.clone(),
                    status: "pending".into(),
                    created: current,
                    account: None,
                });
                state.data.incoming.len() - 1
            }
        }
        "status" => {
            let index = exact
                .filter(|index| !request_is_terminal(&state.data.incoming[*index].status))
                .or_else(|| peer_key_grant(&state.data, &peer, &key))
                .or(exact)
                .ok_or_else(|| invalid("Authorization request not found"))?;
            if state.data.incoming[index].public_key != key {
                return Err(invalid("Authorization key changed"));
            }
            index
        }
        "cancel" => {
            // Cancellation is deliberately idempotent. If the exact request is already gone, a
            // same-key pending record from an interrupted migration can still be cancelled; if no
            // such record exists, the desired end state has already been reached.
            let index = exact
                .filter(|index| !request_is_terminal(&state.data.incoming[*index].status))
                .or_else(|| peer_key_grant(&state.data, &peer, &key))
                .or(exact);
            let Some(index) = index else {
                persist(store, &state.data)?;
                return Ok(Response {
                    version: CONTROL_VERSION,
                    access: None,
                    status: "cancelled".into(),
                    account: None,
                    port: state.server.port,
                    keys: vec![],
                    login_password: String::new(),
                    error: String::new(),
                });
            };
            if state.data.incoming[index].public_key != key {
                return Err(invalid("Authorization key changed"));
            }
            if request_can_be_cancelled(&state.data.incoming[index].status) {
                state.data.incoming[index].status = "cancelled".into();
            }
            index
        }
        _ => unreachable!(),
    };
    let status = state.data.incoming[index].status.clone();
    let account = state.data.incoming[index].account.clone();
    persist(store, &state.data)?;
    let host_keys = if status == "authorized" {
        state.server.host_keys.clone()
    } else {
        vec![]
    };
    // An authorization can outlive a service repair. Keep it authorized in durable state, but
    // report the transient preparation state until strict host verification has a public key.
    let response_status = if status == "authorized" && host_keys.is_empty() {
        "preparing".into()
    } else {
        status
    };
    let login_password = if state.data.incoming[index].status == "authorized" {
        load_or_create_server_password(store, &state.data.incoming[index].id)?
    } else {
        String::new()
    };
    Ok(Response {
        version: CONTROL_VERSION,
        access: Some(access_info(&state.server)),
        status: response_status,
        account,
        port: DEFAULT_SSH_PORT,
        keys: host_keys,
        login_password,
        error: String::new(),
    })
}

fn candidate_address(
    discovered: Option<SocketAddr>,
    cached: Option<&str>,
) -> io::Result<SocketAddr> {
    let ip = discovered
        .map(|a| a.ip())
        .or_else(|| cached.and_then(|s| s.parse::<std::net::IpAddr>().ok()))
        .ok_or_else(|| invalid("Device is offline or not discovered"))?;
    if ip.is_unspecified() || ip.is_multicast() {
        return Err(invalid("Invalid SSH candidate address"));
    }
    Ok(SocketAddr::new(ip, CONTROL_PORT))
}
fn peer_address(
    discovery: Option<&DiscoveryView>,
    fingerprint: CertificateFingerprint,
    cached: Option<&str>,
) -> io::Result<SocketAddr> {
    candidate_address(
        discovery
            .and_then(|d| d.peer(fingerprint))
            .map(|p| p.service_endpoint),
        cached,
    )
}

fn exchange(
    store: &DeviceStore,
    discovery: Option<&DiscoveryView>,
    peer: &str,
    request: &Request,
    cached: Option<&str>,
) -> io::Result<(Response, String)> {
    let fingerprint = CertificateFingerprint::from_str(peer)?;
    let trusted = store.load_peer(fingerprint)?;
    // A cached address is only a candidate when discovery has no current route. The pinned
    // TLS handshake below must still prove possession of this exact paired device identity.
    let address = peer_address(discovery, fingerprint, cached)?;
    let tls = PinnedTlsClient::new(
        &store.load_identity()?,
        trusted.into_certificate_der(),
        fingerprint,
        Duration::from_secs(3),
    )?;
    let mut stream = tls.connect(address)?;
    write_frame(&mut stream, request)?;
    let response: Response = read_frame(&mut stream)?;
    if response.version != CONTROL_VERSION {
        return Err(invalid("Peer version does not support SSH access"));
    }
    stream.conn.send_close_notify();
    let _ = stream.flush();
    if !response.error.is_empty() {
        return Err(io::Error::other(RemoteControlError {
            code: response.error,
        }));
    }
    store.load_peer(fingerprint)?;
    Ok((response, address.ip().to_string()))
}

fn exchange_with_missing_state_recovery<F>(
    request: &Request,
    mut send: F,
) -> (io::Result<(Response, String)>, bool)
where
    F: FnMut(&Request) -> io::Result<(Response, String)>,
{
    let first = send(request);
    let remote_forgot_request = request.action == "status"
        && matches!(
            &first,
            Err(error) if remote_control_error_code(error) == Some("request-not-found")
        );
    if !remote_forgot_request {
        return (first, false);
    }

    // A receiving computer can lose its local state file while the requester still has the
    // private key and approved account. Recreate a pending request with the same id/key instead
    // of leaving both machines in an unrecoverable stale state. Approval remains local and
    // explicit on the receiving computer.
    let mut recovery = request.clone();
    recovery.action = "request".into();
    (send(&recovery), true)
}

fn orphaned_key_candidates(root: &Path, data: &Data) -> io::Result<Vec<(String, String)>> {
    let mut candidates = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(id) = name.strip_suffix(".pub") else {
            continue;
        };
        if !valid_id(id) || data.outgoing.iter().any(|connection| connection.id == id) {
            continue;
        }
        let private = root.join(id);
        let Ok(private_metadata) = fs::symlink_metadata(&private) else {
            continue;
        };
        let public_metadata = entry.metadata()?;
        if !private_metadata.is_file()
            || private_metadata.file_type().is_symlink()
            || public_metadata.len() > 16_384
        {
            continue;
        }
        let Ok(public) = fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(public) = platform::validate_public_key(public.trim()) else {
            continue;
        };
        candidates.push((
            public_metadata.modified().unwrap_or(UNIX_EPOCH),
            id.to_owned(),
            public,
        ));
    }
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.0));
    candidates.truncate(32);
    Ok(candidates
        .into_iter()
        .map(|(_, id, key)| (id, key))
        .collect())
}

fn recover_outgoing_from_orphaned_keys(
    store: &DeviceStore,
    discovery: Option<&DiscoveryView>,
    shared: &Mutex<Snapshot>,
    root: &Path,
    peer: &str,
) -> io::Result<Option<String>> {
    let candidates = {
        let state = shared.lock().unwrap();
        orphaned_key_candidates(root, &state.data)?
    };
    let askpass = platform::keygen_askpass_program()?;
    for (id, public_key) in candidates {
        let request = Request {
            version: CONTROL_VERSION,
            action: "status".into(),
            id: id.clone(),
            key: public_key.clone(),
        };
        let (response, address) = match exchange(store, discovery, peer, &request, None) {
            Ok(result) => result,
            Err(error)
                if matches!(
                    remote_control_error_code(&error),
                    Some("request-not-found" | "key-changed")
                ) =>
            {
                continue;
            }
            Err(error) if remote_control_error_code(&error).is_some() => return Err(error),
            Err(_) => return Ok(None),
        };
        if request_is_terminal(&response.status) {
            continue;
        }
        if !matches!(
            response.status.as_str(),
            "pending" | "granting" | "authorized" | "preparing"
        ) {
            return Err(invalid("Peer returned an invalid SSH authorization state"));
        }
        if response.port != DEFAULT_SSH_PORT {
            return Err(invalid(
                "The target did not return the dedicated Pierdesk SSH port",
            ));
        }
        // The public sidecar is only an index. Derive the key from the private file before using
        // it so a stale or replaced .pub file cannot bind an unrelated private key.
        if matching_client_passphrase(store, &id, &public_key, &askpass).is_err() {
            continue;
        }
        let mut connection = Connection {
            id: id.clone(),
            peer: peer.to_owned(),
            status: response.status,
            public_key,
            host_keys: vec![],
            offered_keys: vec![],
            address,
            port: response.port,
            account: response.account,
            tested: 0,
            admin_verified: false,
            error: String::new(),
            created: now(),
            command: String::new(),
            host_fingerprints: vec![],
            offered_fingerprints: vec![],
        };
        if connection.status == "authorized" {
            install_client_password(
                store,
                &id,
                &response.login_password,
                &connection.public_key,
                &askpass,
            )?;
            connection.host_keys = response.keys;
            save_config(store, &mut connection)?;
        }
        {
            let mut state = shared.lock().unwrap();
            if state
                .data
                .outgoing
                .iter()
                .any(|candidate| candidate.peer == peer && candidate.id == id)
            {
                return Ok(Some(id));
            }
            state.data.outgoing.push(connection);
            persist(store, &state.data)?;
        }
        append_ssh_diagnostic(
            store,
            &format!("{} outgoing_state_recovered id={} peer={}", now(), id, peer),
        );
        return Ok(Some(id));
    }
    Ok(None)
}

fn config_content(root: &Path, c: &Connection) -> io::Result<String> {
    if c.port != DEFAULT_SSH_PORT {
        return Err(invalid("Invalid Pierdesk SSH port"));
    }
    if !valid_id(&c.id) || c.port == 0 || c.address.parse::<std::net::IpAddr>().is_err() {
        return Err(invalid("Invalid SSH connection metadata"));
    }
    let a = c
        .account
        .as_ref()
        .ok_or_else(|| invalid("Account not approved"))?;
    if !valid_account_name(&a.name) {
        return Err(invalid("Unsupported account name"));
    }
    let quote = |p: &Path| -> io::Result<String> {
        let s = p.to_string_lossy().replace('\\', "/");
        if s.contains(['\n', '\r', '"', '%']) {
            return Err(invalid("Unsupported SSH configuration path"));
        }
        Ok(format!("\"{s}\""))
    };
    Ok(format!(
        "Host cf-{}\n  HostName {}\n  ProxyCommand {} ssh-proxy {}\n  User {}\n  Port {}\n  IdentityFile {}\n  UserKnownHostsFile {}\n  GlobalKnownHostsFile none\n  HostKeyAlias cf-{}\n  StrictHostKeyChecking yes\n  IdentitiesOnly yes\n  PasswordAuthentication no\n  KbdInteractiveAuthentication no\n  BatchMode no\n  ForwardAgent no\n  ClearAllForwardings yes\n  ControlMaster no\n  ControlPath none\n  ConnectTimeout 8\n  ServerAliveInterval 15\n  ServerAliveCountMax 2\n",
        c.id,
        format_args!("cf-{}", c.id),
        quote(&root.join("pierdesk-ssh-askpass.exe"))?,
        c.id,
        a.name,
        c.port,
        quote(&root.join(&c.id))?,
        quote(&root.join(format!("{}.known_hosts", c.id)))?,
        c.id
    ))
}

fn normalize_host_keys(keys: &[String]) -> io::Result<Vec<String>> {
    let mut normalized = Vec::with_capacity(keys.len());
    for key in keys {
        let key = platform::normalize_host_public_key(key)?;
        if !normalized.contains(&key) {
            normalized.push(key);
        }
    }
    Ok(normalized)
}

fn save_config(store: &DeviceStore, c: &mut Connection) -> io::Result<()> {
    let root = store.root().join("ssh");
    let host_keys = normalize_host_keys(&c.host_keys)?;
    if host_keys.is_empty() {
        return Err(invalid(
            "SSH host keys unavailable; refresh on the receiving computer",
        ));
    }
    let mut known = String::new();
    for key in &host_keys {
        let _ = writeln!(known, "cf-{} {key}", c.id);
    }
    c.host_keys = host_keys;
    let config = root.join(format!("{}.config", c.id));
    fs::write(root.join(format!("{}.known_hosts", c.id)), known)?;
    let next = root.join(format!("{}.config.next", c.id));
    fs::write(&next, config_content(&root, c)?)?;
    replace_file(&next, &config)?;
    c.command = platform::command_text(&config, &format!("cf-{}", c.id), &c.id)?;
    c.host_fingerprints = c
        .host_keys
        .iter()
        .map(|k| platform::key_fingerprint(k))
        .collect::<io::Result<_>>()?;
    Ok(())
}

/// Records an address only after pinned TLS reached the peer. Existing configs migrate to the
/// local proxy while the display retains the verified address. The proxy resolves each new
/// connection independently; no cached IP can bypass certificate or SSH host-key verification.
fn refresh_config_address(store: &DeviceStore, c: &mut Connection, address: &str) {
    let root = store.root().join("ssh");
    let config = root.join(format!("{}.config", c.id));
    let current = match fs::read_to_string(&config) {
        Ok(current) if c.account.is_some() => current,
        _ => return, // Not provisioned yet; the authorized path writes the first config.
    };
    let previous = std::mem::replace(&mut c.address, address.to_owned());
    let result = config_content(&root, c).and_then(|content| {
        if content == current {
            return Ok(false);
        }
        let next = root.join(format!("{}.config.next", c.id));
        fs::write(&next, content)?;
        replace_file(&next, &config)?;
        Ok(true)
    });
    let outcome = match result {
        Ok(false) if previous == address => return,
        Ok(_) => "ok".to_owned(),
        Err(error) => {
            c.address.clone_from(&previous);
            format!("failed error={}", diagnostic_error(&error.to_string()))
        }
    };
    append_ssh_diagnostic(
        store,
        &format!(
            "{} config_address_refresh id={} from={} to={} result={}",
            now(),
            c.id,
            previous,
            address,
            outcome
        ),
    );
}

#[allow(clippy::too_many_lines)] // Remote response validation and durable state convergence stay atomic.
fn sync_connection(
    store: &DeviceStore,
    discovery: Option<&DiscoveryView>,
    shared: &Mutex<Snapshot>,
    id: &str,
    initial: bool,
) -> io::Result<()> {
    let old = shared
        .lock()
        .unwrap()
        .data
        .outgoing
        .iter()
        .find(|c| c.id == id)
        .cloned()
        .ok_or_else(|| invalid("Connection not found"))?;
    let trusted_host_keys = normalize_host_keys(&old.host_keys)?;
    let request = Request {
        version: CONTROL_VERSION,
        action: synchronization_action(initial).into(),
        id: id.into(),
        key: old.public_key.clone(),
    };
    let (result, remote_state_recovery_attempted) =
        exchange_with_missing_state_recovery(&request, |candidate| {
            exchange(store, discovery, &old.peer, candidate, Some(&old.address))
        });
    let result = result.and_then(|(mut response, address)| {
        if response.status == "authorized" {
            response.keys = normalize_host_keys(&response.keys)?;
            if response.keys.is_empty() {
                // Previous releases could briefly omit the key during startup. Reuse only the
                // identity already trusted by this exact connection. StrictHostKeyChecking in the
                // generated config still rejects a genuinely changed server identity.
                response.keys.clone_from(&trusted_host_keys);
            }
        }
        Ok((response, address))
    });
    if remote_state_recovery_attempted {
        append_ssh_diagnostic(
            store,
            &format!(
                "{} remote_state_recreate id={} peer={} result={}",
                now(),
                id,
                old.peer,
                if result.is_ok() { "accepted" } else { "failed" }
            ),
        );
    }
    let mut state = shared.lock().unwrap();
    if let Ok((response, _)) = &result
        && let Some(access) = &response.access
    {
        state.remote_access.insert(id.into(), access.clone());
    }
    let c = state
        .data
        .outgoing
        .iter_mut()
        .find(|c| c.id == id)
        .ok_or_else(|| invalid("Connection removed"))?;
    let mut sync_error = None;
    match result {
        Ok((response, address)) => {
            refresh_config_address(store, c, &address);
            c.error.clear();
            if response.status == "authorized" {
                if response
                    .access
                    .as_ref()
                    .is_none_or(|a| a.transport != "tunnel-v4" || a.state != "ready")
                {
                    let message = match response.access.as_ref().map(|a| a.state.as_str()) {
                        Some("service-stopped") => {
                            "Target SSH service is stopped; repair local access on the target computer"
                        }
                        Some("rules-missing") => {
                            "Target gateway firewall rule is missing or mismatched; repair local access on the target computer"
                        }
                        _ => {
                            "Target SSH access upgrade required; open Remote work on the target and upgrade local access"
                        }
                    };
                    c.status = "authorized".into();
                    c.account = response.account;
                    c.tested = 0;
                    c.error = message.into();
                    sync_error = Some(invalid(message));
                } else if response.port != DEFAULT_SSH_PORT {
                    let message = "The target did not return the dedicated Pierdesk SSH port";
                    c.status = "error".into();
                    c.account = None;
                    c.tested = 0;
                    c.error = message.into();
                    sync_error = Some(invalid(message));
                } else if c.account.as_ref().map(|a| &a.sid).is_some()
                    && c.account.as_ref().map(|a| &a.sid)
                        != response.account.as_ref().map(|a| &a.sid)
                {
                    let message = "Approved account changed; request a new authorization";
                    c.status = "error".into();
                    c.tested = 0;
                    c.error = message.into();
                    sync_error = Some(invalid(message));
                } else if !valid_login_password(&response.login_password) {
                    let message = "The target did not return a valid independent SSH password";
                    c.status = "error".into();
                    c.tested = 0;
                    c.error = message.into();
                    sync_error = Some(invalid(message));
                } else if let Err(error) = platform::keygen_askpass_program().and_then(|askpass| {
                    install_client_password(
                        store,
                        &c.id,
                        &response.login_password,
                        &c.public_key,
                        &askpass,
                    )
                }) {
                    let message = error.to_string();
                    c.status = "error".into();
                    c.tested = 0;
                    c.error.clone_from(&message);
                    sync_error = Some(invalid(message));
                } else if response.keys.is_empty() {
                    // Compatibility with the previous receiver: it reported authorized while its
                    // startup inspection still had an empty host-key list. Keep polling without
                    // creating an unusable config or showing a terminal error.
                    c.address = address;
                    c.port = response.port;
                    c.account = response.account;
                    c.status = "preparing".into();
                    c.tested = 0;
                } else {
                    c.address = address;
                    c.port = response.port;
                    c.account = response.account;
                    c.host_keys.clone_from(&trusted_host_keys);
                    if !trusted_host_keys.is_empty() && trusted_host_keys != response.keys {
                        c.offered_keys = response.keys;
                        c.offered_fingerprints = c
                            .offered_keys
                            .iter()
                            .map(|k| platform::key_fingerprint(k))
                            .collect::<io::Result<_>>()?;
                        c.status = "hostChanged".into();
                        c.tested = 0;
                    } else {
                        c.host_keys = response.keys;
                        c.offered_keys.clear();
                        c.offered_fingerprints.clear();
                        if c.status != "ready" {
                            c.status = "authorized".into();
                        }
                        save_config(store, c)?;
                    }
                }
            } else if response.status == "preparing" {
                c.address = address;
                c.port = DEFAULT_SSH_PORT;
                c.account = response.account;
                c.status = "preparing".into();
                c.tested = 0;
            } else {
                c.status = response.status;
                c.account = response.account;
                c.tested = 0;
            }
        }
        Err(e) => {
            if let Some(code) = remote_control_error_code(&e) {
                c.error = control_error_message(code).into();
                c.status = match code {
                    "authorization-recovery-required" => "recovery",
                    "existing-authorization" => "conflict",
                    "request-not-found" => "stale",
                    _ => "error",
                }
                .into();
                c.tested = 0;
                sync_error = Some(e);
            } else if e.to_string().contains("SSH host") {
                c.error = e.to_string();
                c.status = "error".into();
                c.tested = 0;
                sync_error = Some(e);
            } else {
                let message = format!(
                    "无法连接对方电脑：{e}。请在两台电脑打开 Pierdesk，在目标电脑的远程工作中检查或修复本机接入；原因未确认。"
                );
                c.error.clone_from(&message);
                c.status = if now().saturating_sub(c.created) > TTL && c.account.is_none() {
                    "expired"
                } else {
                    "offline"
                }
                .into();
                sync_error = Some(invalid(message));
            }
        }
    }
    persist(store, &state.data)?;
    drop(state);
    if let Some(error) = sync_error {
        Err(error)
    } else {
        Ok(())
    }
}

#[allow(clippy::too_many_lines)] // Fixed operation dispatcher; each arm has an explicit authority boundary.
fn execute(
    store: &DeviceStore,
    discovery: Option<&DiscoveryView>,
    shared: &Mutex<Snapshot>,
    command: Command,
) -> io::Result<()> {
    match command {
        Command::Refresh => {
            shared.lock().unwrap().server = platform::inspect(&store.root().join("ssh"))?;
        }
        Command::Setup => {
            platform::setup()?;
            let mut state = shared.lock().unwrap();
            state.server = platform::inspect(&store.root().join("ssh"))?;
        }
        Command::SetupClient => {
            let ssh_root = store.root().join("ssh");
            platform::setup_client(&ssh_root)?;
            shared.lock().unwrap().server = platform::inspect(&ssh_root)?;
        }
        Command::Request(peer) => {
            store.load_peer(CertificateFingerprint::from_str(&peer)?)?;
            let root = store.root().join("ssh");
            if !platform::bundled_client_ready(&root)? {
                platform::setup_client(&root)?;
                shared.lock().unwrap().server = platform::inspect(&root)?;
            }
            platform::protect_directory(&root)?;

            // Retrying from the device card must retain the request id and private key. Creating a
            // fresh pair here leaves the receiver holding an unrelated id that neither side can
            // subsequently cancel or synchronize.
            let existing = {
                let state = shared.lock().unwrap();
                canonical_outgoing_index(&state.data, &peer)
                    .map(|index| state.data.outgoing[index].clone())
            };
            let should_recover_orphaned_state = existing.as_ref().is_none_or(|connection| {
                matches!(
                    connection.status.as_str(),
                    "conflict" | "recovery" | "stale"
                ) && connection.account.is_none()
            });
            if should_recover_orphaned_state
                && recover_outgoing_from_orphaned_keys(store, discovery, shared, &root, &peer)?
                    .is_some()
            {
                return Ok(());
            }
            if let Some(existing) = existing {
                if root.join(&existing.id).is_file() {
                    // A declined, expired, cancelled or revoked record is a finished request, even
                    // when it still carries the account that was approved previously. Reusing its
                    // key is safe, but it must be submitted as a new request so both computers can
                    // leave the terminal state. Treating the saved account as authoritative here
                    // would only poll the old terminal record forever.
                    let initial =
                        explicit_request_is_initial(&existing.status, existing.account.as_ref());
                    if initial {
                        let mut state = shared.lock().unwrap();
                        let connection = state
                            .data
                            .outgoing
                            .iter_mut()
                            .find(|connection| connection.id == existing.id)
                            .ok_or_else(|| invalid("Connection removed"))?;
                        connection.status = "pending".into();
                        connection.account = None;
                        connection.created = now();
                        connection.tested = 0;
                        connection.admin_verified = false;
                        connection.error.clear();
                        persist(store, &state.data)?;
                    }
                    append_ssh_diagnostic(
                        store,
                        &format!("{} request_reused id={} peer={}", now(), existing.id, peer),
                    );
                    sync_connection(store, discovery, shared, &existing.id, initial)?;
                    return Ok(());
                }
                if existing.account.is_some()
                    || matches!(
                        existing.status.as_str(),
                        "granting" | "authorized" | "ready" | "hostChanged"
                    )
                {
                    return Err(invalid(
                        "The local SSH private key is missing; revoke the old authorization on the receiving computer",
                    ));
                }
            }

            {
                let mut state = shared.lock().unwrap();
                if state.data.outgoing.len() >= 128 {
                    state
                        .data
                        .outgoing
                        .retain(|connection| !request_is_terminal(&connection.status));
                }
                if state.data.outgoing.len() >= 128 {
                    return Err(invalid("SSH connection record limit reached"));
                }
            }
            let id = random_id()?;
            let key = platform::generate_key(&root.join(&id))?;
            {
                let mut state = shared.lock().unwrap();
                state.data.outgoing.push(Connection {
                    id: id.clone(),
                    peer: peer.clone(),
                    status: "pending".into(),
                    public_key: key,
                    host_keys: vec![],
                    offered_keys: vec![],
                    address: String::new(),
                    port: DEFAULT_SSH_PORT,
                    account: None,
                    tested: 0,
                    admin_verified: false,
                    error: String::new(),
                    created: now(),
                    command: String::new(),
                    host_fingerprints: vec![],
                    offered_fingerprints: vec![],
                });
                persist(store, &state.data)?;
            }
            append_ssh_diagnostic(
                store,
                &format!("{} request_created id={} peer={}", now(), id, peer),
            );
            sync_connection(store, discovery, shared, &id, true)?;
        }
        Command::Approve { id, sid } => {
            let info = platform::inspect(&store.root().join("ssh"))?;
            if !info.running {
                return Err(invalid("Set up SSH access on this computer first"));
            }
            let account = info
                .accounts
                .iter()
                .find(|a| a.sid == sid)
                .cloned()
                .ok_or_else(|| invalid("Account unavailable"))?;
            if !info.loopback_only || !info.running || info.firewall == "missing" {
                return Err(invalid(
                    "Upgrade or repair local SSH access before approving this request",
                ));
            }
            let grant = {
                let mut state = shared.lock().unwrap();
                if !state.listener_error.is_empty() || !state.local_proxy_error.is_empty() {
                    return Err(invalid(
                        "Pierdesk gateway is unavailable; restart Pierdesk and repair local access",
                    ));
                }
                let g = state
                    .data
                    .incoming
                    .iter_mut()
                    .find(|g| g.id == id)
                    .ok_or_else(|| invalid("Request not found"))?;
                if !approval_can_run(&g.status, g.created, now()) {
                    return Err(invalid("Request is no longer pending"));
                }
                if !approval_matches_started_account(&g.status, g.account.as_ref(), &sid) {
                    return Err(invalid(
                        "Retry authorization from the same Windows account that started it",
                    ));
                }
                g.status = "granting".into();
                g.account = Some(account);
                let g = g.clone();
                persist(store, &state.data)?;
                g
            };
            store.load_peer(CertificateFingerprint::from_str(&grant.peer)?)?;
            let result = platform::grant(&sid, &grant.public_key, &id, false)
                .and_then(|()| load_or_create_server_password(store, &id).map(drop));
            let refreshed = platform::inspect(&store.root().join("ssh")).unwrap_or(info);
            let mut state = shared.lock().unwrap();
            let g = state.data.incoming.iter_mut().find(|g| g.id == id).unwrap();
            // On uncertain elevation outcome, keep a revocable record instead of losing the key.
            g.status = if result.is_ok() {
                "authorized"
            } else {
                "granting"
            }
            .into();
            state.server = refreshed;
            persist(store, &state.data)?;
            result?;
        }
        Command::Reject(id) => {
            let mut s = shared.lock().unwrap();
            let g = s
                .data
                .incoming
                .iter_mut()
                .find(|g| g.id == id)
                .ok_or_else(|| invalid("Request not found"))?;
            if g.status != "pending" {
                return Err(invalid("Request is no longer pending"));
            }
            g.status = "rejected".into();
            persist(store, &s.data)?;
        }
        Command::Revoke(id) => {
            let grant = shared
                .lock()
                .unwrap()
                .data
                .incoming
                .iter()
                .find(|g| g.id == id)
                .cloned()
                .ok_or_else(|| invalid("Grant not found"))?;
            if grant.account.is_none() {
                // No dedicated account means no dedicated authorized_keys file could have been
                // written. This covers interrupted grants and migration markers from the retired
                // Windows-OpenSSH implementation. Clear the record locally without presenting a
                // UAC operation that cannot possibly edit a known authorization file.
                remove_login_password(store, "server", &id)?;
                let mut state = shared.lock().unwrap();
                state
                    .data
                    .incoming
                    .iter_mut()
                    .find(|candidate| candidate.id == id)
                    .ok_or_else(|| invalid("Grant removed"))?
                    .status = "revoked".into();
                persist(store, &state.data)?;
                append_ssh_diagnostic(
                    store,
                    &format!(
                        "{} stale_grant_cleared id={} peer={}",
                        now(),
                        id,
                        grant.peer
                    ),
                );
                return Ok(());
            }
            platform::grant(
                &grant
                    .account
                    .ok_or_else(|| invalid("No account on authorization"))?
                    .sid,
                &grant.public_key,
                &id,
                true,
            )?;
            remove_login_password(store, "server", &id)?;
            let mut s = shared.lock().unwrap();
            s.data
                .incoming
                .iter_mut()
                .find(|g| g.id == id)
                .unwrap()
                .status = "revoked".into();
            persist(store, &s.data)?;
        }
        Command::Cancel(id) => {
            let c = {
                let mut state = shared.lock().unwrap();
                let connection = state
                    .data
                    .outgoing
                    .iter_mut()
                    .find(|connection| connection.id == id)
                    .ok_or_else(|| invalid("Connection not found"))?;
                if connection.account.is_some() {
                    return Err(invalid(
                        "Revoke this authorization on the receiving computer",
                    ));
                }
                let copy = connection.clone();
                // Commit the user's desired local state before attempting the network cleanup. A
                // powered-off peer or an old peer that forgot the id must never trap the request in
                // an uncancellable state.
                connection.status = "cancelled".into();
                connection.tested = 0;
                connection.error.clear();
                persist(store, &state.data)?;
                copy
            };
            let result = exchange(
                store,
                discovery,
                &c.peer,
                &Request {
                    version: CONTROL_VERSION,
                    action: "cancel".into(),
                    id: id.clone(),
                    key: c.public_key,
                },
                Some(&c.address),
            );
            match result {
                Ok((reply, _))
                    if matches!(
                        reply.status.as_str(),
                        "cancelled" | "expired" | "rejected" | "revoked" | "superseded"
                    ) => {}
                Ok((reply, _)) if matches!(reply.status.as_str(), "granting" | "authorized") => {
                    let mut state = shared.lock().unwrap();
                    let connection = state
                        .data
                        .outgoing
                        .iter_mut()
                        .find(|connection| connection.id == id)
                        .ok_or_else(|| invalid("Connection removed"))?;
                    connection.status = reply.status;
                    connection.account = reply.account;
                    persist(store, &state.data)?;
                    return Err(invalid(
                        "Authorization has already started; finish or revoke it on the receiving computer",
                    ));
                }
                Ok(_) => {
                    // The remote state can lag behind the cancel response. The receiver treats a
                    // repeated cancel as idempotent, so the local cancelled state remains correct.
                }
                Err(error) => {
                    append_ssh_diagnostic(
                        store,
                        &format!(
                            "{} cancel_cleanup_deferred id={} peer={} details={}",
                            now(),
                            id,
                            c.peer,
                            diagnostic_error(&error.to_string())
                        ),
                    );
                }
            }
        }
        Command::TrustHost { id, expected } => {
            let mut s = shared.lock().unwrap();
            let c = s
                .data
                .outgoing
                .iter_mut()
                .find(|c| c.id == id)
                .ok_or_else(|| invalid("Connection not found"))?;
            if c.status != "hostChanged" || c.offered_keys.join("\n") != expected {
                return Err(invalid("No new host identity to trust"));
            }
            c.host_keys = std::mem::take(&mut c.offered_keys);
            c.status = "authorized".into();
            c.tested = 0;
            save_config(store, c)?;
            persist(store, &s.data)?;
        }
        Command::Test(id) => {
            perform_connection(store, discovery, shared, &id, false)?;
        }
        Command::Terminal(id) => {
            perform_connection(store, discovery, shared, &id, true)?;
        }
        Command::Disconnect(id) => {
            if let Some(tunnel) = shared
                .lock()
                .unwrap()
                .tunnels
                .iter()
                .find(|t| t.session.id == id)
            {
                tunnel.cancel.store(true, Ordering::Release);
            }
        }
    }
    Ok(())
}

fn perform_connection(
    store: &DeviceStore,
    discovery: Option<&DiscoveryView>,
    shared: &Mutex<Snapshot>,
    id: &str,
    terminal: bool,
) -> io::Result<()> {
    sync_connection(store, discovery, shared, id, false)?;
    let c = shared
        .lock()
        .unwrap()
        .data
        .outgoing
        .iter()
        .find(|c| c.id == id)
        .cloned()
        .ok_or_else(|| invalid("Connection not found"))?;
    store.load_peer(CertificateFingerprint::from_str(&c.peer)?)?;
    if !matches!(c.status.as_str(), "authorized" | "ready" | "offline")
        || c.account.is_none()
        || c.host_keys.is_empty()
        || !c.offered_keys.is_empty()
    {
        return Err(invalid(
            "Connection is not authorized or the host identity requires review",
        ));
    }
    let config = store.root().join("ssh").join(format!("{id}.config"));
    let alias = format!("cf-{id}");
    let account = c.account.ok_or_else(|| invalid("Account unavailable"))?;
    let result = platform::test(&config, &alias, &account.sid, id);
    let mut s = shared.lock().unwrap();
    let c = s.data.outgoing.iter_mut().find(|c| c.id == id).unwrap();
    match result {
        Ok(value) => {
            let admin = value["admin"].as_bool().unwrap_or(false);
            if admin != account.admin {
                c.status = "error".into();
                c.tested = 0;
                persist(store, &s.data)?;
                return Err(invalid(
                    "Remote account privilege changed; revoke and authorize again",
                ));
            }
            c.status = "ready".into();
            c.tested = now();
            c.admin_verified = admin;
            c.error.clear();
        }
        Err(e) => {
            c.status = "authorized".into();
            c.tested = 0;
            c.error = e.to_string();
            persist(store, &s.data)?;
            return Err(e);
        }
    }
    persist(store, &s.data)?;
    drop(s);
    if terminal {
        platform::terminal(&config, &alias, id)?;
    }
    Ok(())
}

/// End-to-end check of the client key passphrase lifecycle through the real `ssh-keygen` and the
/// `askpass` executable (a `Pierdesk` build): DPAPI-protected credentials encrypt the key,
/// passphrases never travel on a command line, rotation works, and a helper that cannot run is
/// reported as a tool failure rather than as a credential that no longer unlocks the key.
/// Unit tests cannot cover this because the test harness cannot act as the helper; it runs from
/// the integration tests and from `pierdesk ssh-keygen-self-test` before packaging.
#[doc(hidden)]
pub fn keygen_askpass_self_test(askpass: &Path) -> io::Result<()> {
    fn check(condition: bool, failure: &str) -> io::Result<()> {
        if condition {
            Ok(())
        } else {
            Err(invalid(format!("ssh-keygen self-test: {failure}")))
        }
    }
    let unlocks = |key_path: &Path, passphrase: &str| {
        platform::public_key(key_path, passphrase, askpass)
            .and_then(|key| platform::validate_public_key(&key))
    };
    let root = std::env::temp_dir().join(format!("pierdesk-keygen-self-test-{}", random_id()?));
    let result = (|| {
        let store = DeviceStore::new(root.join("store"));
        let ssh_root = store.root().join("ssh");
        fs::create_dir_all(&ssh_root)?;
        platform::setup_client(&ssh_root)?;
        let id = random_id()?;
        let key_path = ssh_root.join(&id);
        let public_key = platform::generate_key(&key_path)?;

        let first = generate_login_password()?;
        install_client_password(&store, &id, &first, &public_key, askpass)?;
        check(
            read_login_password(&store, "client", &id)? == first,
            "first credential was not saved",
        )?;
        check(
            unlocks(&key_path, &first)? == public_key,
            "first passphrase does not unlock the key",
        )?;
        check(
            unlocks(&key_path, "").is_err_and(|error| platform::is_passphrase_mismatch(&error)),
            "an empty passphrase was not reported as a mismatch",
        )?;
        let protected = fs::read(credential_path(&store, "client", &id)?)?;
        check(
            !protected
                .windows(first.len())
                .any(|window| window == first.as_bytes()),
            "credential file contains the plaintext passphrase",
        )?;

        let second = generate_login_password()?;
        install_client_password(&store, &id, &second, &public_key, askpass)?;
        check(
            read_login_password(&store, "client", &id)? == second,
            "rotated credential was not saved",
        )?;
        check(
            unlocks(&key_path, &first).is_err_and(|error| platform::is_passphrase_mismatch(&error)),
            "the previous passphrase still unlocks the key",
        )?;
        check(
            unlocks(&key_path, &second)? == public_key,
            "rotated passphrase does not unlock the key",
        )?;

        let missing = ssh_root.join("missing-askpass.exe");
        check(
            platform::public_key(&key_path, &second, &missing)
                .is_err_and(|error| !platform::is_passphrase_mismatch(&error)),
            "a helper that cannot run was reported as a wrong passphrase",
        )?;
        check(
            matching_client_passphrase(&store, &id, &public_key, &missing).is_err_and(|error| {
                error
                    .to_string()
                    .starts_with("Cannot run ssh-keygen to verify the local SSH key")
            }),
            "a helper that cannot run was reported as an unusable credential",
        )?;
        let leftovers = fs::read_dir(&ssh_root)?
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".keygen-"))
            .count();
        check(
            leftovers == 0,
            "per-invocation secret files were left behind",
        )
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[cfg(test)]
mod tests {
    #[test]
    fn persisted_credential_magic_keeps_its_original_name() {
        // Stored DPAPI credential blobs start with this label; a different one rejects them all.
        assert_eq!(CREDENTIAL_MAGIC, "ClipFerry SSH credential v1");
    }

    use super::*;
    struct TestRoot(std::path::PathBuf);
    impl TestRoot {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("pierdesk-ssh-test-{}", random_id().unwrap()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn test_key_with(byte: u8) -> String {
        let mut wire = b"\0\0\0\x0bssh-ed25519\0\0\0\x20".to_vec();
        wire.extend_from_slice(&[byte; 32]);
        format!("ssh-ed25519 {}", platform::base64(&wire))
    }
    fn test_key() -> String {
        test_key_with(1)
    }

    fn test_connection(peer: &str, status: &str, created: u64) -> Connection {
        Connection {
            id: random_id().unwrap(),
            peer: peer.into(),
            status: status.into(),
            public_key: test_key(),
            host_keys: vec![],
            offered_keys: vec![],
            address: "192.168.1.2".into(),
            port: DEFAULT_SSH_PORT,
            account: None,
            tested: 0,
            admin_verified: false,
            error: String::new(),
            created,
            command: String::new(),
            host_fingerprints: vec![],
            offered_fingerprints: vec![],
        }
    }

    #[test]
    fn errored_connections_retry_with_capped_backoff() {
        assert!(connection_retries_after_error(
            "error",
            "private key cannot be unlocked"
        ));
        assert!(!connection_retries_after_error(
            "error",
            HOST_KEYS_UNAVAILABLE
        ));
        assert!(!connection_retries_after_error("ready", ""));
        assert!(!connection_retries_after_error("cancelled", ""));

        let start = Instant::now();
        let mut schedule = ErrorRetrySchedule::default();
        assert!(
            schedule.take_due("a", start),
            "first observation retries at once"
        );
        assert!(!schedule.take_due("a", start + Duration::from_secs(29)));
        let mut clock = start + ERROR_RETRY_INITIAL;
        let mut gaps = Vec::new();
        for _ in 0..7 {
            assert!(schedule.take_due("a", clock));
            let gap = schedule.0["a"].1;
            gaps.push(gap.as_secs());
            assert!(!schedule.take_due("a", clock + gap.saturating_sub(Duration::from_secs(1))));
            clock += gap;
        }
        assert_eq!(gaps, [60, 120, 240, 480, 600, 600, 600]);

        schedule.retain(|id| id != "a");
        assert!(
            schedule.take_due("a", clock),
            "a new error starts a fresh backoff"
        );
    }

    #[test]
    fn verified_address_rewrites_only_an_existing_config() {
        let root = TestRoot::new();
        let store = DeviceStore::new(root.0.join("store"));
        let ssh_root = store.root().join("ssh");
        fs::create_dir_all(&ssh_root).unwrap();
        let mut c = test_connection("peer", "error", now());
        c.address = "192.168.1.13".into();

        refresh_config_address(&store, &mut c, "192.168.1.11");
        assert_eq!(
            c.address, "192.168.1.13",
            "unprovisioned connection stays untouched"
        );

        c.account = Some(platform::Account {
            name: "user".into(),
            sid: "S-1-5-21-1".into(),
            admin: false,
        });
        let config = ssh_root.join(format!("{}.config", c.id));
        fs::write(&config, config_content(&ssh_root, &c).unwrap()).unwrap();
        refresh_config_address(&store, &mut c, "192.168.1.11");
        let written = fs::read_to_string(&config).unwrap();
        assert!(written.contains(&format!("  HostName cf-{}\n", c.id)));
        assert!(written.contains(" ssh-proxy "));
        assert!(!written.contains("192.168.1.11"));
        assert!(written.contains(&format!("  HostKeyAlias cf-{}\n", c.id)));
        assert_eq!(c.address, "192.168.1.11");
        let log = fs::read_to_string(store.root().join("logs/ssh.log")).unwrap();
        assert!(log.contains("from=192.168.1.13 to=192.168.1.11 result=ok"));

        refresh_config_address(&store, &mut c, "192.168.1.11");
        let log_after = fs::read_to_string(store.root().join("logs/ssh.log")).unwrap();
        assert_eq!(
            log, log_after,
            "an unchanged address is not rewritten or logged"
        );
    }

    #[test]
    fn background_sync_logs_only_failure_changes_and_recovery() {
        let root = TestRoot::new();
        let store = DeviceStore::new(root.0.join("store"));
        let mut last = HashMap::new();
        let offline = || Err(invalid("Device is offline or not discovered"));
        record_background_sync(&store, &mut last, "a", &offline());
        record_background_sync(&store, &mut last, "a", &offline());
        record_background_sync(&store, &mut last, "a", &Ok(()));
        record_background_sync(&store, &mut last, "a", &Ok(()));
        let log = fs::read_to_string(store.root().join("logs/ssh.log")).unwrap();
        assert_eq!(log.matches("background_sync result=failed id=a").count(), 1);
        assert_eq!(
            log.matches("background_sync result=recovered id=a").count(),
            1
        );
    }

    #[test]
    fn generated_login_passwords_are_high_entropy_canonical_values() {
        let passwords = (0..128)
            .map(|_| generate_login_password().unwrap())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(passwords.len(), 128);
        assert!(
            passwords
                .iter()
                .all(|password| valid_login_password(password))
        );
        assert!(
            passwords
                .iter()
                .all(|password| password.len() == LOGIN_PASSWORD_LENGTH)
        );
    }

    // The passphrase lifecycle through the real ssh-keygen needs a Pierdesk build as its askpass
    // helper, so it lives in `keygen_askpass_self_test` (tests/ssh_keygen_askpass.rs).

    #[test]
    fn background_sync_polls_while_explicit_retry_submits() {
        let account = platform::Account {
            name: "user".into(),
            sid: "S-1-5-21-1".into(),
            admin: false,
        };
        assert_eq!(synchronization_action(false), "status");
        assert_eq!(synchronization_action(true), "request");
        assert!(explicit_request_is_initial("revoked", Some(&account)));
        assert!(explicit_request_is_initial("pending", None));
        assert!(!explicit_request_is_initial("authorized", Some(&account)));
    }

    #[test]
    fn canonical_outgoing_matches_the_last_active_record_shown_by_the_ui() {
        let peer = "peer";
        let data = Data {
            schema: STATE_SCHEMA_VERSION,
            incoming: vec![],
            outgoing: vec![
                test_connection(peer, "ready", 30),
                test_connection(peer, "error", 20),
                test_connection(peer, "cancelled", 40),
            ],
        };
        assert_eq!(canonical_outgoing_index(&data, peer), Some(1));
    }

    #[test]
    fn old_control_protocol_is_rejected_and_terminal_status_is_not_revived_by_polling() {
        let root = TestRoot::new();
        let store = DeviceStore::new(root.0.join("receiver"));
        store.load_or_create_identity().unwrap();
        fs::create_dir_all(store.root().join("ssh")).unwrap();
        let sender = DeviceStore::new(root.0.join("sender"))
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
        let id = random_id().unwrap();
        let shared = Mutex::new(Snapshot {
            data: Data {
                schema: STATE_SCHEMA_VERSION,
                incoming: vec![Grant {
                    id: id.clone(),
                    peer: sender.fingerprint().to_string(),
                    label: "sender".into(),
                    public_key: test_key(),
                    status: "rejected".into(),
                    created: now(),
                    account: None,
                }],
                outgoing: vec![],
            },
            ..Snapshot::default()
        });
        let poll = Request {
            version: CONTROL_VERSION,
            action: "status".into(),
            id: id.clone(),
            key: test_key(),
        };
        assert_eq!(
            respond(&store, &shared, sender.fingerprint(), &poll)
                .unwrap()
                .status,
            "rejected"
        );
        assert_eq!(shared.lock().unwrap().data.incoming[0].status, "rejected");

        shared.lock().unwrap().data.incoming[0].status = "migration".into();
        let retry = Request {
            version: CONTROL_VERSION,
            action: "request".into(),
            id: id.clone(),
            key: test_key(),
        };
        assert_eq!(
            respond(&store, &shared, sender.fingerprint(), &retry)
                .unwrap()
                .status,
            "pending"
        );

        let old = Request { version: 1, ..poll };
        let error = respond(&store, &shared, sender.fingerprint(), &old).unwrap_err();
        assert_eq!(control_error_code(&error), "version-mismatch");
    }

    #[test]
    fn expired_request_stays_expired_when_polled() {
        let root = TestRoot::new();
        let store = DeviceStore::new(root.0.join("receiver"));
        store.load_or_create_identity().unwrap();
        fs::create_dir_all(store.root().join("ssh")).unwrap();
        let sender = DeviceStore::new(root.0.join("sender"))
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
        let id = random_id().unwrap();
        let shared = Mutex::new(Snapshot {
            data: Data {
                schema: STATE_SCHEMA_VERSION,
                incoming: vec![Grant {
                    id: id.clone(),
                    peer: sender.fingerprint().to_string(),
                    label: "sender".into(),
                    public_key: test_key(),
                    status: "pending".into(),
                    created: now().saturating_sub(TTL + 1),
                    account: None,
                }],
                outgoing: vec![],
            },
            ..Snapshot::default()
        });
        let response = respond(
            &store,
            &shared,
            sender.fingerprint(),
            &Request {
                version: CONTROL_VERSION,
                action: "status".into(),
                id,
                key: test_key(),
            },
        )
        .unwrap();
        assert_eq!(response.status, "expired");
        assert_eq!(shared.lock().unwrap().data.incoming[0].status, "expired");
    }

    #[test]
    fn interrupted_grant_can_be_retried_after_request_ttl() {
        assert!(approval_can_run("pending", 100, 100 + TTL));
        assert!(!approval_can_run("pending", 100, 101 + TTL));
        assert!(approval_can_run("granting", 100, 101 + TTL));
        assert!(!approval_can_run("authorized", 100, 100));
        let account = platform::Account {
            name: "user".into(),
            sid: "S-1-5-21-1".into(),
            admin: false,
        };
        assert!(approval_matches_started_account(
            "granting",
            None,
            &account.sid
        ));
        assert!(approval_matches_started_account(
            "granting",
            Some(&account),
            &account.sid
        ));
        assert!(!approval_matches_started_account(
            "granting",
            Some(&account),
            "S-1-5-21-2"
        ));
        assert!(connection_needs_sync("granting", ""));
        assert!(connection_needs_sync("preparing", ""));
        assert!(connection_needs_sync("error", HOST_KEYS_UNAVAILABLE));
        assert!(!connection_needs_sync("error", "another error"));
        assert!(!connection_needs_sync("conflict", ""));
    }

    #[test]
    fn stale_grant_without_an_account_can_be_cleared_locally() {
        let root = TestRoot::new();
        let store = DeviceStore::new(root.0.join("receiver"));
        store.load_or_create_identity().unwrap();
        fs::create_dir_all(store.root().join("ssh")).unwrap();
        let id = random_id().unwrap();
        let shared = Mutex::new(Snapshot {
            data: Data {
                schema: STATE_SCHEMA_VERSION,
                incoming: vec![Grant {
                    id: id.clone(),
                    peer: "0".repeat(64),
                    label: "sender".into(),
                    public_key: test_key(),
                    status: "granting".into(),
                    created: now(),
                    account: None,
                }],
                outgoing: vec![],
            },
            ..Snapshot::default()
        });

        execute(&store, None, &shared, Command::Revoke(id.clone())).unwrap();

        assert_eq!(
            shared
                .lock()
                .unwrap()
                .data
                .incoming
                .iter()
                .find(|grant| grant.id == id)
                .unwrap()
                .status,
            "revoked"
        );
    }

    #[test]
    fn cancelling_an_undiscovered_peer_still_commits_local_cancellation() {
        let root = TestRoot::new();
        let store = DeviceStore::new(root.0.join("sender"));
        store.load_or_create_identity().unwrap();
        fs::create_dir_all(store.root().join("ssh")).unwrap();
        let receiver = DeviceStore::new(root.0.join("receiver"))
            .load_or_create_identity()
            .unwrap()
            .identity;
        store
            .trust_peer(
                receiver.certificate_der().to_vec(),
                receiver.fingerprint(),
                "receiver",
            )
            .unwrap();
        let id = random_id().unwrap();
        let shared = Mutex::new(Snapshot {
            data: Data {
                schema: STATE_SCHEMA_VERSION,
                incoming: vec![],
                outgoing: vec![Connection {
                    id: id.clone(),
                    peer: receiver.fingerprint().to_string(),
                    status: "offline".into(),
                    public_key: test_key(),
                    host_keys: vec![],
                    offered_keys: vec![],
                    address: String::new(),
                    port: 22,
                    account: None,
                    tested: 0,
                    admin_verified: false,
                    error: "offline".into(),
                    created: now(),
                    command: String::new(),
                    host_fingerprints: vec![],
                    offered_fingerprints: vec![],
                }],
            },
            ..Snapshot::default()
        });

        execute(&store, None, &shared, Command::Cancel(id.clone())).unwrap();

        let state = shared.lock().unwrap();
        let connection = state
            .data
            .outgoing
            .iter()
            .find(|connection| connection.id == id)
            .unwrap();
        assert_eq!(connection.status, "cancelled");
        assert!(connection.error.is_empty());
    }

    #[test]
    fn requests_are_peer_bound_idempotent_and_never_self_approve() {
        let root = TestRoot::new();
        let store = DeviceStore::new(root.0.join("receiver"));
        store.load_or_create_identity().unwrap();
        fs::create_dir_all(store.root().join("ssh")).unwrap();
        let a = DeviceStore::new(root.0.join("a"))
            .load_or_create_identity()
            .unwrap()
            .identity;
        let b = DeviceStore::new(root.0.join("b"))
            .load_or_create_identity()
            .unwrap()
            .identity;
        store
            .trust_peer(a.certificate_der().to_vec(), a.fingerprint(), "A")
            .unwrap();
        store
            .trust_peer(b.certificate_der().to_vec(), b.fingerprint(), "B")
            .unwrap();
        let shared = Mutex::new(Snapshot::default());
        let mut request = Request {
            version: CONTROL_VERSION,
            action: "request".into(),
            id: random_id().unwrap(),
            key: test_key(),
        };
        assert_eq!(
            respond(&store, &shared, a.fingerprint(), &request)
                .unwrap()
                .status,
            "pending"
        );
        assert_eq!(
            respond(&store, &shared, a.fingerprint(), &request)
                .unwrap()
                .status,
            "pending"
        );
        assert_eq!(shared.lock().unwrap().data.incoming.len(), 1);
        request.action = "approve".into();
        assert!(respond(&store, &shared, a.fingerprint(), &request).is_err());
        request.action = "status".into();
        assert!(respond(&store, &shared, b.fingerprint(), &request).is_err());
        request.action = "cancel".into();
        assert_eq!(
            respond(&store, &shared, a.fingerprint(), &request)
                .unwrap()
                .status,
            "cancelled"
        );
        request.action = "request".into();
        assert_eq!(
            respond(&store, &shared, a.fingerprint(), &request)
                .unwrap()
                .status,
            "pending"
        );
        request.id = random_id().unwrap();
        assert_eq!(
            respond(&store, &shared, a.fingerprint(), &request)
                .unwrap()
                .status,
            "pending"
        );
        shared
            .lock()
            .unwrap()
            .data
            .incoming
            .last_mut()
            .unwrap()
            .created = now() - TTL - 1;
        assert_eq!(
            respond(&store, &shared, a.fingerprint(), &request)
                .unwrap()
                .status,
            "pending"
        );
        request.version = CONTROL_VERSION - 1;
        assert!(respond(&store, &shared, a.fingerprint(), &request).is_err());
        store.revoke_peer(a.fingerprint()).unwrap();
        request.version = CONTROL_VERSION;
        assert!(respond(&store, &shared, a.fingerprint(), &request).is_err());
    }

    #[test]
    fn newer_unapproved_request_supersedes_stale_pending_record() {
        let root = TestRoot::new();
        let store = DeviceStore::new(root.0.join("receiver"));
        store.load_or_create_identity().unwrap();
        fs::create_dir_all(store.root().join("ssh")).unwrap();
        let sender = DeviceStore::new(root.0.join("sender"))
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
        let shared = Mutex::new(Snapshot::default());
        let first_id = random_id().unwrap();
        let second_id = random_id().unwrap();
        respond(
            &store,
            &shared,
            sender.fingerprint(),
            &Request {
                version: CONTROL_VERSION,
                action: "request".into(),
                id: first_id.clone(),
                key: test_key_with(1),
            },
        )
        .unwrap();
        let response = respond(
            &store,
            &shared,
            sender.fingerprint(),
            &Request {
                version: CONTROL_VERSION,
                action: "request".into(),
                id: second_id.clone(),
                key: test_key_with(2),
            },
        )
        .unwrap();
        assert_eq!(response.status, "pending");
        let state = shared.lock().unwrap();
        assert_eq!(
            state
                .data
                .incoming
                .iter()
                .find(|grant| grant.id == first_id)
                .unwrap()
                .status,
            "superseded"
        );
        assert_eq!(
            state
                .data
                .incoming
                .iter()
                .find(|grant| grant.id == second_id)
                .unwrap()
                .status,
            "pending"
        );
    }

    #[test]
    fn same_device_and_key_reassociate_an_active_record_after_local_id_loss() {
        let root = TestRoot::new();
        let store = DeviceStore::new(root.0.join("receiver"));
        store.load_or_create_identity().unwrap();
        fs::create_dir_all(store.root().join("ssh")).unwrap();
        let sender = DeviceStore::new(root.0.join("sender"))
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
        let original_id = random_id().unwrap();
        let reconstructed_id = random_id().unwrap();
        let key = test_key();
        let account = platform::Account {
            name: "administrator".into(),
            sid: "S-1-5-21-1".into(),
            admin: true,
        };
        let shared = Mutex::new(Snapshot {
            data: Data {
                schema: STATE_SCHEMA_VERSION,
                incoming: vec![Grant {
                    id: original_id.clone(),
                    peer: sender.fingerprint().to_string(),
                    label: "sender".into(),
                    public_key: key.clone(),
                    status: "authorized".into(),
                    created: now(),
                    account: Some(account.clone()),
                }],
                outgoing: vec![],
            },
            ..Snapshot::default()
        });

        let preparing = respond(
            &store,
            &shared,
            sender.fingerprint(),
            &Request {
                version: CONTROL_VERSION,
                action: "status".into(),
                id: reconstructed_id.clone(),
                key: key.clone(),
            },
        )
        .unwrap();
        assert_eq!(preparing.status, "preparing");
        assert!(preparing.keys.is_empty());
        assert!(valid_login_password(&preparing.login_password));
        let login_password = preparing.login_password;
        assert_eq!(shared.lock().unwrap().data.incoming[0].status, "authorized");
        shared.lock().unwrap().server = platform::ServerInfo {
            installed: true,
            running: true,
            port: DEFAULT_SSH_PORT,
            host_keys: vec![key.clone()],
            ..platform::ServerInfo::default()
        };

        for action in ["request", "status"] {
            let response = respond(
                &store,
                &shared,
                sender.fingerprint(),
                &Request {
                    version: CONTROL_VERSION,
                    action: action.into(),
                    id: reconstructed_id.clone(),
                    key: key.clone(),
                },
            )
            .unwrap();
            assert_eq!(response.status, "authorized");
            assert_eq!(response.login_password, login_password);
            assert_eq!(
                response.account.as_ref().map(|item| &item.sid),
                Some(&account.sid)
            );
        }
        let state = shared.lock().unwrap();
        assert_eq!(state.data.incoming.len(), 1);
        assert_eq!(state.data.incoming[0].id, original_id);
    }

    #[test]
    fn missing_remote_state_is_recreated_with_the_same_request_and_key() {
        let root = TestRoot::new();
        let store = DeviceStore::new(root.0.join("receiver"));
        store.load_or_create_identity().unwrap();
        fs::create_dir_all(store.root().join("ssh")).unwrap();
        let sender = DeviceStore::new(root.0.join("sender"))
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
        let shared = Mutex::new(Snapshot::default());
        let id = random_id().unwrap();
        let key = test_key();
        let status = Request {
            version: CONTROL_VERSION,
            action: "status".into(),
            id: id.clone(),
            key: key.clone(),
        };
        let mut actions = Vec::new();
        let (result, attempted) = exchange_with_missing_state_recovery(&status, |request| {
            actions.push(request.action.clone());
            respond(&store, &shared, sender.fingerprint(), request)
                .map(|response| (response, "192.168.1.29".into()))
                .map_err(|error| {
                    io::Error::other(RemoteControlError {
                        code: control_error_code(&error).into(),
                    })
                })
        });

        assert!(attempted);
        assert_eq!(actions, ["status", "request"]);
        assert_eq!(result.unwrap().0.status, "pending");
        let state = shared.lock().unwrap();
        assert_eq!(state.data.incoming.len(), 1);
        assert_eq!(state.data.incoming[0].id, id);
        assert_eq!(state.data.incoming[0].public_key, key);
    }

    #[test]
    fn orphaned_private_key_pairs_are_available_for_state_recovery() {
        let root = TestRoot::new();
        let ssh_root = root.0.join("ssh");
        fs::create_dir_all(&ssh_root).unwrap();
        let id = random_id().unwrap();
        fs::write(ssh_root.join(&id), b"private-key-placeholder").unwrap();
        fs::write(
            ssh_root.join(format!("{id}.pub")),
            format!("{} pierdesk\n", test_key()),
        )
        .unwrap();

        let candidates = orphaned_key_candidates(&ssh_root, &Data::default()).unwrap();
        assert_eq!(candidates, vec![(id.clone(), test_key())]);

        let known = Data {
            schema: STATE_SCHEMA_VERSION,
            incoming: vec![],
            outgoing: vec![Connection {
                id,
                peer: "1".repeat(64),
                status: "pending".into(),
                public_key: test_key(),
                host_keys: vec![],
                offered_keys: vec![],
                address: String::new(),
                port: 22,
                account: None,
                tested: 0,
                admin_verified: false,
                error: String::new(),
                created: now(),
                command: String::new(),
                host_fingerprints: vec![],
                offered_fingerprints: vec![],
            }],
        };
        assert!(
            orphaned_key_candidates(&ssh_root, &known)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn cancellation_is_idempotent_and_recovers_a_mismatched_pending_id() {
        let root = TestRoot::new();
        let store = DeviceStore::new(root.0.join("receiver"));
        store.load_or_create_identity().unwrap();
        fs::create_dir_all(store.root().join("ssh")).unwrap();
        let sender = DeviceStore::new(root.0.join("sender"))
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
        let shared = Mutex::new(Snapshot::default());
        let key = test_key();
        let original_id = random_id().unwrap();
        respond(
            &store,
            &shared,
            sender.fingerprint(),
            &Request {
                version: CONTROL_VERSION,
                action: "request".into(),
                id: original_id.clone(),
                key: key.clone(),
            },
        )
        .unwrap();
        let cancel = Request {
            version: CONTROL_VERSION,
            action: "cancel".into(),
            id: random_id().unwrap(),
            key: key.clone(),
        };
        assert_eq!(
            respond(&store, &shared, sender.fingerprint(), &cancel)
                .unwrap()
                .status,
            "cancelled"
        );
        assert_eq!(
            shared
                .lock()
                .unwrap()
                .data
                .incoming
                .iter()
                .find(|grant| grant.id == original_id)
                .unwrap()
                .status,
            "cancelled"
        );
        assert_eq!(
            respond(&store, &shared, sender.fingerprint(), &cancel)
                .unwrap()
                .status,
            "cancelled"
        );

        // A terminal duplicate must not hide a same-key authorization that has already entered
        // its privileged phase. Report the active record so the requester cannot claim success.
        shared.lock().unwrap().data.incoming.push(Grant {
            id: random_id().unwrap(),
            peer: sender.fingerprint().to_string(),
            label: "sender".into(),
            public_key: key.clone(),
            status: "granting".into(),
            created: now(),
            account: None,
        });
        assert_eq!(
            respond(&store, &shared, sender.fingerprint(), &cancel)
                .unwrap()
                .status,
            "granting"
        );

        let missing = Request {
            id: random_id().unwrap(),
            key: test_key_with(3),
            ..cancel
        };
        assert_eq!(
            respond(&store, &shared, sender.fingerprint(), &missing)
                .unwrap()
                .status,
            "cancelled"
        );
    }

    #[test]
    fn granting_record_requires_local_recovery_instead_of_silent_replacement() {
        let root = TestRoot::new();
        let store = DeviceStore::new(root.0.join("receiver"));
        store.load_or_create_identity().unwrap();
        fs::create_dir_all(store.root().join("ssh")).unwrap();
        let sender = DeviceStore::new(root.0.join("sender"))
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
        let shared = Mutex::new(Snapshot {
            data: Data {
                schema: STATE_SCHEMA_VERSION,
                incoming: vec![Grant {
                    id: random_id().unwrap(),
                    peer: sender.fingerprint().to_string(),
                    label: "sender".into(),
                    public_key: test_key(),
                    status: "granting".into(),
                    created: now() - TTL - 1,
                    account: None,
                }],
                outgoing: vec![],
            },
            ..Snapshot::default()
        });
        let error = respond(
            &store,
            &shared,
            sender.fingerprint(),
            &Request {
                version: CONTROL_VERSION,
                action: "request".into(),
                id: random_id().unwrap(),
                key: test_key_with(2),
            },
        )
        .expect_err("granting record should block a replacement request");
        assert_eq!(
            control_error_code(&error),
            "authorization-recovery-required"
        );
    }

    #[test]
    fn diagnostic_errors_are_single_line_bounded_and_redact_key_material() {
        let key = "A".repeat(120);
        let message = format!("first\r\nssh-ed25519 {key} {}", "中".repeat(300));
        let safe = diagnostic_error(&message);
        assert!(!safe.contains(['\r', '\n']));
        assert!(!safe.contains(&key));
        assert!(safe.contains("[redacted]"));
        assert!(safe.len() <= 512);
    }

    #[test]
    fn ssh_control_request_roundtrips_over_real_pinned_mutual_tls() {
        let root = TestRoot::new();
        let store = DeviceStore::new(root.0.join("receiver"));
        let server_identity = store.load_or_create_identity().unwrap().identity;
        let client_identity = DeviceStore::new(root.0.join("sender"))
            .load_or_create_identity()
            .unwrap()
            .identity;
        store
            .trust_peer(
                client_identity.certificate_der().to_vec(),
                client_identity.fingerprint(),
                "sender",
            )
            .unwrap();
        fs::create_dir_all(store.root().join("ssh")).unwrap();
        let client = PinnedTlsClient::new(
            &client_identity,
            server_identity.certificate_der().to_vec(),
            server_identity.fingerprint(),
            Duration::from_secs(3),
        )
        .unwrap();
        let server =
            TrustedTlsServer::new(server_identity, store.clone(), Duration::from_secs(3)).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || {
            let shared = Arc::new(Mutex::new(Snapshot::default()));
            for _ in 0..2 {
                let (socket, _) = listener.accept().unwrap();
                serve_connection(&store, &shared, &server, socket, &AtomicBool::new(false))
                    .unwrap();
            }
        });
        let first_id = random_id().unwrap();
        let mut stream = client.connect(address).unwrap();
        write_frame(
            &mut stream,
            &Request {
                version: CONTROL_VERSION,
                action: "request".into(),
                id: first_id,
                key: test_key(),
            },
        )
        .unwrap();
        let response: Response = read_frame(&mut stream).unwrap();
        assert_eq!(response.status, "pending");
        assert!(response.account.is_none());
        assert!(response.keys.is_empty());
        assert!(response.error.is_empty());
        let mut eof = [0_u8; 1];
        assert_eq!(
            stream.read(&mut eof).unwrap(),
            0,
            "missing TLS close_notify"
        );

        let mut stream = client.connect(address).unwrap();
        write_frame(
            &mut stream,
            &Request {
                version: CONTROL_VERSION,
                action: "request".into(),
                id: random_id().unwrap(),
                key: test_key(),
            },
        )
        .unwrap();
        let response: Response = read_frame(&mut stream).unwrap();
        assert_eq!(response.status, "pending");
        assert!(response.error.is_empty());
        assert_eq!(
            stream.read(&mut eof).unwrap(),
            0,
            "missing TLS close_notify"
        );
        worker.join().unwrap();
    }

    #[test]
    fn persisted_state_recovers_from_the_previous_valid_backup() {
        let root = TestRoot::new();
        let store = DeviceStore::new(root.0.join("store"));
        fs::create_dir_all(store.root().join("ssh")).unwrap();
        let first_id = random_id().unwrap();
        let first = Data {
            schema: STATE_SCHEMA_VERSION,
            incoming: vec![Grant {
                id: first_id.clone(),
                peer: "1".repeat(64),
                label: "first".into(),
                public_key: test_key(),
                status: "pending".into(),
                created: now(),
                account: None,
            }],
            outgoing: vec![],
        };
        persist(&store, &first).unwrap();
        let second = Data {
            schema: STATE_SCHEMA_VERSION,
            incoming: vec![Grant {
                id: random_id().unwrap(),
                peer: "2".repeat(64),
                label: "second".into(),
                public_key: test_key_with(2),
                status: "pending".into(),
                created: now(),
                account: None,
            }],
            outgoing: vec![],
        };
        persist(&store, &second).unwrap();
        fs::write(store.root().join("ssh/state.json"), vec![0_u8; 4096]).unwrap();

        let (recovered, warning) = load_persisted_state(&store);

        assert!(warning.is_some());
        assert_eq!(recovered.incoming.len(), 1);
        assert_eq!(recovered.incoming[0].id, first_id);
        assert_eq!(
            read_state_file(&store.root().join("ssh/state.json"))
                .unwrap()
                .incoming[0]
                .id,
            first_id
        );
    }

    #[test]
    fn corrupt_state_without_a_backup_starts_with_explicit_empty_recovery() {
        let root = TestRoot::new();
        let store = DeviceStore::new(root.0.join("store"));
        fs::create_dir_all(store.root().join("ssh")).unwrap();
        fs::write(store.root().join("ssh/state.json"), vec![0_u8; 4096]).unwrap();

        let (recovered, warning) = load_persisted_state(&store);

        assert!(recovered.incoming.is_empty());
        assert!(recovered.outgoing.is_empty());
        assert!(warning.unwrap().contains("no usable backup"));
    }

    #[test]
    fn actual_windows_keygen_and_ssh_config_parser_accept_spaced_paths() {
        let root = TestRoot::new();
        let directory = root.0.join("private keys");
        platform::protect_directory(&directory).unwrap();
        let id = random_id().unwrap();
        let key = platform::generate_key(&directory.join(&id)).unwrap();
        assert_eq!(platform::validate_public_key(&key).unwrap(), key);
        let mut command =
            std::process::Command::new(platform::ssh_tool(&directory, "ssh-keygen.exe").unwrap());
        command
            .args(["-lf"])
            .arg(directory.join(format!("{id}.pub")));
        let fingerprint = platform::output(command, Duration::from_secs(5)).unwrap();
        assert!(fingerprint.contains(&platform::key_fingerprint(&key).unwrap()));
        let c = Connection {
            id,
            peer: String::new(),
            status: "authorized".into(),
            public_key: key.clone(),
            host_keys: vec![key],
            offered_keys: vec![],
            address: "127.0.0.1".into(),
            port: DEFAULT_SSH_PORT,
            account: Some(platform::Account {
                name: "测试用户".into(),
                sid: "S-1-5-21-1".into(),
                admin: false,
            }),
            tested: 0,
            admin_verified: false,
            error: String::new(),
            created: 0,
            command: String::new(),
            host_fingerprints: vec![],
            offered_fingerprints: vec![],
        };
        let config = directory.join("test.config");
        fs::write(&config, config_content(&directory, &c).unwrap()).unwrap();
        let mut command =
            std::process::Command::new(platform::ssh_tool(&directory, "ssh.exe").unwrap());
        command
            .arg("-G")
            .arg("-F")
            .arg(config)
            .arg(format!("cf-{}", c.id));
        let output = platform::output(command, Duration::from_secs(5)).unwrap();
        assert!(output.contains(&format!("hostname cf-{}", c.id)));
        assert!(output.contains("ssh-proxy"));
        assert!(output.contains("port 45834"));
        assert!(output.contains("user 测试用户"));
        assert!(output.contains("passwordauthentication no"));
    }

    #[test]
    fn save_config_accepts_and_removes_an_openssh_host_key_comment() {
        let root = TestRoot::new();
        let store = DeviceStore::new(root.0.join("store"));
        fs::create_dir_all(store.root().join("ssh")).unwrap();
        let mut connection = test_connection("peer", "authorized", 0);
        connection.address = "127.0.0.1".into();
        connection.account = Some(platform::Account {
            name: "tester".into(),
            sid: "S-1-5-21-1".into(),
            admin: false,
        });
        connection.host_keys = vec![format!("{} user@LAPTOP-OFFICE", test_key())];

        save_config(&store, &mut connection).unwrap();

        assert_eq!(connection.host_keys, vec![test_key()]);
        let known_hosts = fs::read_to_string(
            store
                .root()
                .join(format!("ssh/{}.known_hosts", connection.id)),
        )
        .unwrap();
        assert_eq!(
            known_hosts,
            format!("cf-{} {}\n", connection.id, test_key())
        );
    }
    #[test]
    fn bounds_precede_allocation() {
        let mut input = &[0xff, 0xff, 0xff, 0xff][..];
        assert!(read_frame::<Request>(&mut input).is_err());
    }
    #[test]
    fn rejects_unknown_fields_and_versions_in_wire_schema() {
        let raw = br#"{"version":1,"action":"request","id":"a","key":"b","script":"x"}"#;
        assert!(serde_json::from_slice::<Request>(raw).is_err());
    }
    #[test]
    fn random_ids_are_valid_and_distinct() {
        let a = random_id().unwrap();
        let b = random_id().unwrap();
        assert!(valid_id(&a));
        assert_ne!(a, b);
        assert!(!valid_id("../foo"));
    }

    #[test]
    fn account_names_accept_unicode_letters_without_allowing_config_injection() {
        for name in ["测试用户", "Renée", "用户_1001", "12345", "Administrator"] {
            assert!(valid_account_name(name), "expected {name:?} to be accepted");
        }
        for name in [
            "",
            "name with space",
            "name\nProxyCommand",
            "name/other",
            "账户@域",
        ] {
            assert!(
                !valid_account_name(name),
                "expected {name:?} to be rejected"
            );
        }
    }
    #[test]
    fn config_never_disables_host_authentication() {
        let c = Connection {
            id: "a".repeat(32),
            peer: String::new(),
            status: "authorized".into(),
            public_key: String::new(),
            host_keys: vec![],
            offered_keys: vec![],
            address: "192.168.1.2".into(),
            port: DEFAULT_SSH_PORT,
            account: Some(platform::Account {
                name: "dev".into(),
                sid: "S-1-5-21-1".into(),
                admin: false,
            }),
            tested: 0,
            admin_verified: false,
            error: String::new(),
            created: 0,
            command: String::new(),
            host_fingerprints: vec![],
            offered_fingerprints: vec![],
        };
        let text = config_content(Path::new("C:/a b"), &c).unwrap();
        assert!(text.contains("StrictHostKeyChecking yes"));
        assert!(text.contains("PasswordAuthentication no"));
        assert!(text.contains("IdentityFile \"C:/a b/"));
        let mut wrong_port = c.clone();
        wrong_port.port = 22;
        assert!(config_content(Path::new("C:/safe"), &wrong_port).is_err());
        let mut bad = c;
        bad.address = "host\nProxyCommand bad".into();
        assert!(config_content(Path::new("C:/safe"), &bad).is_err());
    }

    #[test]
    fn legacy_state_cannot_expose_port_22_or_an_old_connection_command() {
        let account = platform::Account {
            name: "Example".into(),
            sid: "S-1-5-21-1".into(),
            admin: true,
        };
        let mut outgoing = test_connection("peer-a", "ready", 10);
        outgoing.port = 22;
        outgoing.account = Some(account.clone());
        outgoing.tested = 123;
        outgoing.admin_verified = true;
        outgoing.command = "ssh -p 22 old-host".into();
        outgoing.host_keys = vec![test_key()];
        let mut data = Data {
            schema: 0,
            incoming: vec![Grant {
                id: "1".repeat(32),
                peer: "peer-a".into(),
                label: "A".into(),
                public_key: test_key(),
                status: "authorized".into(),
                created: 1,
                account: Some(account),
            }],
            outgoing: vec![outgoing],
        };

        assert!(migrate_to_dedicated_runtime(&mut data, 99));
        assert_eq!(data.schema, STATE_SCHEMA_VERSION);
        assert_eq!(data.incoming[0].status, "migration");
        assert!(data.incoming[0].account.is_none());
        let migrated = &data.outgoing[0];
        assert_eq!(migrated.status, "migration");
        assert_eq!(migrated.port, DEFAULT_SSH_PORT);
        assert!(migrated.account.is_none());
        assert_eq!(migrated.tested, 0);
        assert!(!migrated.admin_verified);
        assert!(migrated.command.is_empty());
        assert!(migrated.host_keys.is_empty());
        assert!(!migrate_to_dedicated_runtime(&mut data, 100));
    }
}
