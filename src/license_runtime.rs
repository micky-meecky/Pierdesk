//! Application licensing policy. Trust anchors are compiled in, never read from user settings.
//!
//! Without the `licensing` feature this is the free community build: access is always granted,
//! no background worker runs and nothing is sent over the network.
#[cfg(feature = "licensing")]
use crate::{device_store::DeviceStore, licensing::LicenseClient};
use serde::Serialize;
use std::io;
#[cfg(feature = "licensing")]
use std::{
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

/// Whether this build carries the license gate and its settings page.
pub const ENABLED: bool = cfg!(feature = "licensing");

/// Official builds inject the real server through `PIERDESK_LICENSE_ENDPOINT` at compile time.
/// The `.invalid` fallback can never resolve, so a build without it cannot reach any server.
#[cfg(feature = "licensing")]
pub const ENDPOINT: &str = match option_env!("PIERDESK_LICENSE_ENDPOINT") {
    Some(endpoint) => endpoint,
    None => "https://license.invalid",
};
#[cfg(feature = "licensing")]
pub const ISSUER: &str = include_str!("../licensing/test-public-key.txt");

#[cfg(feature = "licensing")]
/// Opens the application license store with the compiled trust anchor.
/// # Errors
/// Returns an error if the user data path or trust anchor is invalid.
pub fn client() -> io::Result<LicenseClient> {
    LicenseClient::new(
        DeviceStore::current_user()?.root().join("licensing-test"),
        ISSUER.trim().to_owned(),
    )
}

#[cfg(feature = "licensing")]
/// Product entry points call this independently of any `WebView` state.
/// # Errors
/// Fails closed for absent, invalid, revoked or expired licenses.
pub fn require_access() -> io::Result<()> {
    if cfg!(test) {
        return Ok(());
    }
    client()?.status().map(|_| ()).map_err(|_| io::Error::new(io::ErrorKind::PermissionDenied,
        "请在设置的“激活与授权”中激活，或联网验证已有授权。 / Activate or verify your license in Settings."))
}

/// Community build: no license gate.
/// # Errors
/// Never fails.
#[cfg(not(feature = "licensing"))]
#[allow(clippy::unnecessary_wraps)] // Keeps the call sites identical to the licensed build.
pub const fn require_access() -> io::Result<()> {
    Ok(())
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::struct_excessive_bools)] // Independent display facts sent to the settings UI.
pub struct Snapshot {
    pub active: bool,
    pub busy: bool,
    pub license_id: String,
    pub offline_until: u64,
    pub message: String,
    pub pending_release: bool,
    pub has_license: bool,
}

#[cfg(feature = "licensing")]
// Do not derive Debug: the activation code must never appear in diagnostics.
pub enum Command {
    Activate(zeroize::Zeroizing<String>),
    Refresh,
    Release,
}

#[cfg(feature = "licensing")]
pub struct Runtime {
    sender: mpsc::SyncSender<(u64, Command)>,
    receiver: mpsc::Receiver<(u64, Snapshot)>,
    next_request: u64,
    pending_request: Option<u64>,
    reported_active: bool,
    pub snapshot: Snapshot,
}

#[cfg(feature = "licensing")]
impl Runtime {
    /// Starts the background verifier without blocking the tray on network calls.
    /// # Errors
    /// Returns an error if the license store or worker cannot be initialized.
    pub fn start() -> io::Result<Self> {
        let client = client()?;
        let snapshot = snapshot_for(&client, String::new());
        let (sender, commands) = mpsc::sync_channel(1);
        let (updates, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("license-worker".into())
            .spawn(move || {
                let mut refresh_at = Instant::now();
                let mut message = String::new();
                loop {
                    let operation = match commands.recv_timeout(Duration::from_secs(5)) {
                        Ok(command) => Some(command),
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => None,
                    };
                    let operation = operation.or_else(|| {
                        (Instant::now() >= refresh_at && client.has_cached_license())
                            .then_some((0, Command::Refresh))
                    });
                    let request_id = operation.as_ref().map_or(0, |(id, _)| *id);
                    if let Some((_, command)) = operation {
                        let result = match command {
                            Command::Activate(code) => {
                                client.activate(ENDPOINT, code.trim()).map(|_| "activated")
                            }
                            Command::Refresh if client.has_pending_release() => {
                                client.release(ENDPOINT).map(|()| "released")
                            }
                            Command::Refresh => client.refresh(ENDPOINT).map(|_| "verified"),
                            Command::Release => client.release(ENDPOINT).map(|()| "released"),
                        };
                        refresh_at = Instant::now()
                            + Duration::from_secs(if result.is_ok() { 6 * 3600 } else { 300 });
                        message = result.map_or_else(|error| safe_error(&error), str::to_owned);
                    }
                    if updates
                        .send((request_id, snapshot_for(&client, message.clone())))
                        .is_err()
                    {
                        break;
                    }
                }
            })?;
        Ok(Self {
            sender,
            receiver,
            reported_active: snapshot.active,
            snapshot,
            next_request: 1,
            pending_request: None,
        })
    }

    /// Queues one user operation, with secrets kept out of diagnostics.
    /// # Errors
    /// Returns an error when an operation is pending or the worker is unavailable.
    pub fn command(&mut self, command: Command) -> io::Result<()> {
        if self.snapshot.busy {
            return Err(io::Error::other(
                "授权操作正在进行 / License operation in progress",
            ));
        }
        let releasing = matches!(command, Command::Release);
        let id = self.next_request;
        self.sender
            .try_send((id, command))
            .map_err(|_| io::Error::other("授权服务忙，请稍后重试 / Please retry"))?;
        self.next_request += 1;
        self.pending_request = Some(id);
        if releasing {
            self.snapshot.active = false;
            self.snapshot.pending_release = true;
        }
        self.snapshot.busy = true;
        "working".clone_into(&mut self.snapshot.message);
        Ok(())
    }

    pub fn poll(&mut self) -> bool {
        while let Ok((id, snapshot)) = self.receiver.try_recv() {
            if self.pending_request.is_some_and(|pending| pending != id) {
                continue;
            }
            self.pending_request = None;
            self.snapshot = snapshot;
        }
        // Expiry is enforced even if an online request is stalled.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(u64::MAX, |v| v.as_secs());
        if now >= self.snapshot.offline_until {
            self.snapshot.active = false;
        }
        let changed = self.reported_active != self.snapshot.active;
        self.reported_active = self.snapshot.active;
        changed
    }
}

/// Community build runtime: permanently "active", no worker, no network.
#[cfg(not(feature = "licensing"))]
pub struct Runtime {
    pub snapshot: Snapshot,
}

#[cfg(not(feature = "licensing"))]
impl Runtime {
    /// # Errors
    /// Never fails; the signature matches the licensed build.
    #[allow(clippy::unnecessary_wraps)]
    pub fn start() -> io::Result<Self> {
        Ok(Self {
            snapshot: Snapshot {
                active: true,
                ..Snapshot::default()
            },
        })
    }

    #[allow(clippy::unused_self)]
    pub const fn poll(&mut self) -> bool {
        false
    }
}

#[cfg(all(test, not(feature = "licensing")))]
mod community_tests {
    use super::*;

    #[test]
    fn community_build_never_gates_or_changes_state() {
        require_access().unwrap();
        let mut runtime = Runtime::start().unwrap();
        assert!(runtime.snapshot.active);
        assert!(!runtime.poll());
        assert!(runtime.snapshot.active);
    }
}

#[cfg(feature = "licensing")]
fn snapshot_for(client: &LicenseClient, message: String) -> Snapshot {
    let mut state = Snapshot {
        message,
        pending_release: client.has_pending_release(),
        has_license: client.has_cached_license(),
        ..Snapshot::default()
    };
    match client.status() {
        Ok(license) => {
            state.active = true;
            state.license_id = license.claims.license_id;
            state.offline_until = license.claims.offline_until;
        }
        Err(error) if state.message.is_empty() => state.message = safe_error(&error),
        Err(_) => {}
    }
    if !state.active
        && let Ok(license) = client.cached_metadata()
    {
        state.license_id = license.claims.license_id;
        state.offline_until = license.claims.offline_until;
    }
    state
}

#[cfg(feature = "licensing")]
fn safe_error(error: &io::Error) -> String {
    let message = error.to_string();
    for (needle, code) in [
        ("bound_elsewhere", "bound_elsewhere"),
        ("unknown_code", "unknown_code"),
        ("device_mismatch", "revoked"),
        ("unknown_license", "revoked"),
        ("rate_limited", "rate_limited"),
        ("clock moved", "clock_error"),
        ("validity window", "expired"),
        ("release pending", "release_pending"),
        ("released or revoked", "revoked"),
    ] {
        if message.contains(needle) {
            return code.into();
        }
    }
    if error.kind() == io::ErrorKind::NotFound {
        "not_activated"
    } else if error.kind() == io::ErrorKind::InvalidData {
        "invalid_local_data"
    } else {
        "network_or_storage_error"
    }
    .into()
}

#[cfg(feature = "licensing")]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_worker_updates_cannot_finish_another_operation_or_reenable_release() {
        let (sender, commands) = mpsc::sync_channel(1);
        let (updates, receiver) = mpsc::channel();
        let active = Snapshot {
            active: true,
            offline_until: u64::MAX,
            has_license: true,
            ..Snapshot::default()
        };
        let mut runtime = Runtime {
            sender,
            receiver,
            snapshot: active.clone(),
            next_request: 1,
            pending_request: None,
            reported_active: true,
        };
        updates.send((0, active)).unwrap();
        runtime.command(Command::Release).unwrap();
        assert!(runtime.poll());
        assert!(!runtime.snapshot.active);
        assert!(runtime.snapshot.busy);
        assert!(runtime.command(Command::Refresh).is_err());
        let (request, _) = commands.try_recv().unwrap();
        updates.send((request, Snapshot::default())).unwrap();
        assert!(!runtime.poll());
        assert!(!runtime.snapshot.busy);
    }

    #[test]
    fn signed_offline_deadline_is_enforced_while_online_operation_is_pending() {
        let (sender, _commands) = mpsc::sync_channel(1);
        let (_updates, receiver) = mpsc::channel();
        let mut runtime = Runtime {
            sender,
            receiver,
            snapshot: Snapshot {
                active: true,
                busy: true,
                offline_until: 1,
                ..Snapshot::default()
            },
            next_request: 2,
            pending_request: Some(1),
            reported_active: true,
        };
        assert!(runtime.poll());
        assert!(!runtime.snapshot.active);
        assert!(runtime.snapshot.busy);
    }
}
