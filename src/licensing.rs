//! Device-bound licensing client and signed offline cache.

use std::fs::{self, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clipferry_license_protocol::{
    ActivateRequest, ErrorResponse, RefreshRequest, ReleaseRequest, ReleaseResponse, SignedLicense,
    generate_device_key, hex_encode, public_key, sign_refresh, sign_release, verify_license,
    verify_license_signature,
};
use getrandom::fill;
use url::Url;
use windows::Win32::Storage::FileSystem::{
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};
use windows_core::PCWSTR;
use zeroize::Zeroizing;

use crate::device_store::{
    protect_for_current_user, read_bounded, unprotect_for_current_user, write_new_atomic,
};

const DEVICE_FILE: &str = "license-device.dpapi";
const LICENSE_FILE: &str = "license-v1.json";
const KEY_LIMIT: usize = 8192;
const LICENSE_LIMIT: usize = 8192;

#[derive(Clone, Debug)]
pub struct LicenseClient {
    root: PathBuf,
    issuer_public_key: String,
}

impl LicenseClient {
    #[must_use]
    pub fn has_cached_license(&self) -> bool {
        self.root.join(LICENSE_FILE).exists()
    }

    #[must_use]
    pub fn has_pending_release(&self) -> bool {
        self.root.join("release-pending.json").exists()
    }
    /// Creates a client for a local data directory and trusted issuer key.
    ///
    /// # Errors
    /// Returns an error when the issuer key is malformed.
    pub fn new(root: impl Into<PathBuf>, issuer_public_key: String) -> io::Result<Self> {
        clipferry_license_protocol::hex_decode::<32>(&issuer_public_key)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        Ok(Self {
            root: root.into(),
            issuer_public_key,
        })
    }

    /// Returns the public half of this machine's DPAPI-protected license identity.
    ///
    /// # Errors
    /// Returns an error when key creation, decryption, or storage fails.
    pub fn device_public_key(&self) -> io::Result<String> {
        let key = self.load_or_create_device_key()?;
        public_key(&key).map_err(invalid_data)
    }

    /// Binds a code and saves the signed license after verification.
    ///
    /// # Errors
    /// Returns an error for network, server, signature, or local-storage failures.
    pub fn activate(&self, endpoint: &str, code: &str) -> io::Result<SignedLicense> {
        let device_public_key = self.device_public_key()?;
        let request = ActivateRequest {
            activation_code: code.to_owned(),
            device_public_key: device_public_key.clone(),
        };
        let license: SignedLicense = send_json(endpoint, "activate", &request)?;
        self.validate_and_save(&license, &device_public_key)?;
        Ok(license)
    }

    /// Renews the signed license with proof of possession of the device key.
    ///
    /// # Errors
    /// Returns an error for a missing or invalid license, network failure, or server rejection.
    pub fn refresh(&self, endpoint: &str) -> io::Result<SignedLicense> {
        let current = self.load_signed_license()?;
        let device_private_key = self.load_device_key()?;
        let device_public_key = public_key(&device_private_key).map_err(invalid_data)?;
        verify_license_signature(&current, &self.issuer_public_key, &device_public_key)
            .map_err(invalid_data)?;
        let mut nonce = [0; 16];
        fill(&mut nonce).map_err(|error| io::Error::other(error.to_string()))?;
        let nonce = hex_encode(&nonce);
        let license_id = current.claims.license_id.clone();
        let request = RefreshRequest {
            license_id: license_id.clone(),
            device_public_key,
            signature: sign_refresh(&license_id, &nonce, &device_private_key)
                .map_err(invalid_data)?,
            nonce,
        };
        let license: SignedLicense = match send_json(endpoint, "refresh", &request) {
            Ok(license) => license,
            Err(error) => {
                if error.to_string().contains("(device_mismatch)")
                    || error.to_string().contains("(unknown_license)")
                {
                    self.block_binding(&current)?;
                }
                return Err(error);
            }
        };
        if license.claims.license_id != license_id
            || license.claims.binding_version < current.claims.binding_version
        {
            return Err(invalid_data(
                "refresh returned a different or older license binding",
            ));
        }
        self.validate_and_save(&license, &request.device_public_key)?;
        Ok(license)
    }

    /// Loads and verifies the locally cached, currently valid license.
    ///
    /// # Errors
    /// Returns an error when the license is absent, corrupt, expired, or belongs to another device.
    pub fn status(&self) -> io::Result<SignedLicense> {
        let license = self.load_signed_license()?;
        self.check_local_state(&license)?;
        let device_public_key = public_key(&self.load_device_key()?).map_err(invalid_data)?;
        verify_license(
            &license,
            &self.issuer_public_key,
            &device_public_key,
            now_secs()?,
        )
        .map_err(invalid_data)?;
        Ok(license)
    }

    /// Returns verified cache metadata even when its offline window has expired.
    /// # Errors
    /// Returns an error for missing, foreign, or tampered cache data.
    pub fn cached_metadata(&self) -> io::Result<SignedLicense> {
        let license = self.load_signed_license()?;
        let device = public_key(&self.load_device_key()?).map_err(invalid_data)?;
        verify_license_signature(&license, &self.issuer_public_key, &device)
            .map_err(invalid_data)?;
        Ok(license)
    }

    /// Releases the current binding online; a persisted request supports interrupted retries.
    /// # Errors
    /// Returns a network, proof, or storage error. A pending release disables local use.
    pub fn release(&self, endpoint: &str) -> io::Result<()> {
        let license = self.load_signed_license()?;
        let key = self.load_device_key()?;
        let device = public_key(&key).map_err(invalid_data)?;
        verify_license_signature(&license, &self.issuer_public_key, &device)
            .map_err(invalid_data)?;
        let pending = self.root.join("release-pending.json");
        let mut request: ReleaseRequest = if pending.exists() {
            serde_json::from_slice(&read_bounded(&pending, LICENSE_LIMIT, "pending release")?)
                .map_err(io::Error::other)?
        } else {
            let mut nonce = [0; 16];
            fill(&mut nonce).map_err(|error| io::Error::other(error.to_string()))?;
            ReleaseRequest {
                license_id: license.claims.license_id.clone(),
                device_public_key: device.clone(),
                binding_version: license.claims.binding_version,
                timestamp: now_secs()?,
                nonce: hex_encode(&nonce),
                signature: String::new(),
            }
        };
        if request.license_id != license.claims.license_id
            || request.device_public_key != device
            || request.binding_version != license.claims.binding_version
        {
            return Err(invalid_data("pending release does not match binding"));
        }
        request.timestamp = now_secs()?;
        sign_release(&mut request, &key).map_err(invalid_data)?;
        write_replace_atomic(&pending, &serde_json::to_vec(&request)?)?;
        match send_json::<_, ReleaseResponse>(endpoint, "release", &request) {
            Ok(receipt) => {
                if receipt.license_id != request.license_id
                    || receipt.binding_version != request.binding_version
                    || receipt.nonce != request.nonce
                {
                    return Err(invalid_data("invalid release receipt"));
                }
            }
            // An administrator may have already reset or disabled this exact old binding.
            Err(error) if error.to_string().contains("(device_mismatch)") => {}
            Err(error) => return Err(error),
        }
        self.block_binding(&license)?;
        fs::remove_file(pending)?;
        fs::remove_file(self.root.join(LICENSE_FILE))?;
        Ok(())
    }

    fn block_binding(&self, license: &SignedLicense) -> io::Result<()> {
        let path = self.root.join("blocked-binding.dpapi");
        let bytes = serde_json::to_vec(&(
            license.claims.license_id.clone(),
            license.claims.binding_version,
        ))?;
        write_replace_atomic(&path, &protect_for_current_user(&bytes)?)
    }

    fn check_local_state(&self, license: &SignedLicense) -> io::Result<()> {
        if self.root.join("release-pending.json").exists() {
            return Err(invalid_data("release pending; reconnect to complete"));
        }
        let path = self.root.join("blocked-binding.dpapi");
        if path.exists() {
            let value =
                unprotect_for_current_user(&read_bounded(&path, KEY_LIMIT, "blocked binding")?)?;
            let (id, version): (String, u64) = serde_json::from_slice(&value)?;
            if id == license.claims.license_id && license.claims.binding_version <= version {
                return Err(invalid_data("binding has been released or revoked"));
            }
        }
        let path = self.root.join("clock.dpapi");
        let now = now_secs()?;
        let last = if path.exists() {
            let value =
                unprotect_for_current_user(&read_bounded(&path, KEY_LIMIT, "license clock")?)?;
            serde_json::from_slice::<u64>(&value)?
        } else {
            0
        };
        if now.saturating_add(300) < last {
            return Err(invalid_data("clock moved backwards; correct system time"));
        }
        if now > last.saturating_add(60) {
            write_replace_atomic(
                &path,
                &protect_for_current_user(&serde_json::to_vec(&now)?)?,
            )?;
        }
        Ok(())
    }

    fn load_signed_license(&self) -> io::Result<SignedLicense> {
        let bytes = read_bounded(&self.root.join(LICENSE_FILE), LICENSE_LIMIT, "license")?;
        let license: SignedLicense = serde_json::from_slice(&bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Ok(license)
    }

    fn validate_and_save(
        &self,
        license: &SignedLicense,
        device_public_key: &str,
    ) -> io::Result<()> {
        verify_license(
            license,
            &self.issuer_public_key,
            device_public_key,
            now_secs()?,
        )
        .map_err(invalid_data)?;
        fs::create_dir_all(&self.root)?;
        self.check_local_state(license)?;
        let bytes = serde_json::to_vec(license)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if bytes.len() > LICENSE_LIMIT {
            return Err(invalid_data("license too large"));
        }
        write_replace_atomic(&self.root.join(LICENSE_FILE), &bytes)
    }

    fn load_or_create_device_key(&self) -> io::Result<Zeroizing<Vec<u8>>> {
        fs::create_dir_all(&self.root)?;
        let path = self.root.join(DEVICE_FILE);
        match self.load_device_key() {
            Ok(key) => return Ok(key),
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            Err(_) => {}
        }
        let key = Zeroizing::new(generate_device_key().map_err(invalid_data)?);
        let protected = protect_for_current_user(&key)?;
        match write_new_atomic(&path, &protected) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        self.load_device_key()
    }

    fn load_device_key(&self) -> io::Result<Zeroizing<Vec<u8>>> {
        let protected = read_bounded(
            &self.root.join(DEVICE_FILE),
            KEY_LIMIT,
            "protected license device key",
        )?;
        let key = unprotect_for_current_user(&protected)?;
        public_key(&key).map_err(invalid_data)?;
        Ok(key)
    }
}

fn send_json<T: serde::Serialize, R: serde::de::DeserializeOwned>(
    endpoint: &str,
    operation: &str,
    request: &T,
) -> io::Result<R> {
    let url = server_url(endpoint, operation)?;
    let response = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout(Duration::from_secs(10))
        .build()
        .post(&url)
        .set("Content-Type", "application/json")
        .send_json(request);
    let response = match response {
        Ok(response) => response,
        Err(ureq::Error::Status(status, response)) => {
            let code = read_response_bytes(response)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<ErrorResponse>(&bytes).ok())
                .map(|body| body.code)
                .filter(|code| {
                    matches!(
                        code.as_str(),
                        "invalid_request"
                            | "unknown_code"
                            | "bound_elsewhere"
                            | "unknown_license"
                            | "device_mismatch"
                            | "replayed_nonce"
                            | "internal_error"
                            | "rate_limited"
                    )
                })
                .unwrap_or_else(|| "server_rejected_request".to_owned());
            return Err(io::Error::other(format!(
                "license request failed: HTTP {status} ({code})"
            )));
        }
        Err(error) => return Err(io::Error::other(format!("license request failed: {error}"))),
    };
    if response.status() != 200 {
        return Err(invalid_data(&format!(
            "unexpected license response: HTTP {} (redirects are disabled)",
            response.status()
        )));
    }
    let bytes = read_response_bytes(response)?;
    serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn read_response_bytes(response: ureq::Response) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take((LICENSE_LIMIT + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > LICENSE_LIMIT {
        return Err(invalid_data("license response too large"));
    }
    Ok(bytes)
}

fn server_url(endpoint: &str, operation: &str) -> io::Result<String> {
    let parsed =
        Url::parse(endpoint).map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let local_http =
        parsed.scheme() == "http" && matches!(parsed.host_str(), Some("127.0.0.1" | "localhost"));
    if parsed.scheme() != "https" && !local_http {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "HTTPS is required",
        ));
    }
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.path() != "/"
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "server URL must contain only the origin",
        ));
    }
    Ok(format!(
        "{}/v1/{operation}",
        parsed.origin().ascii_serialization()
    ))
}

fn write_replace_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut suffix = [0; 8];
    fill(&mut suffix).map_err(|error| io::Error::other(error.to_string()))?;
    let temporary = path.with_extension(format!("{}.tmp", hex_encode(&suffix)));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let written = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = written {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    let source: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let result = unsafe {
        MoveFileExW(
            PCWSTR(source.as_ptr()),
            PCWSTR(destination.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(io::Error::other(error.to_string()));
    }
    Ok(())
}

fn now_secs() -> io::Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_secs())
}

fn invalid_data(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipferry_license_protocol::{
        LicenseClaims, PRODUCT, PROTOCOL_VERSION, sign_license, verify_refresh,
    };
    use std::io::{BufRead as _, BufReader};
    use std::net::TcpListener;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Instant;

    fn fixture() -> (tempfile::TempDir, Vec<u8>, LicenseClient, SignedLicense) {
        let root = tempfile::tempdir().unwrap();
        let issuer = generate_device_key().unwrap();
        let client = LicenseClient::new(root.path(), public_key(&issuer).unwrap()).unwrap();
        let device = client.device_public_key().unwrap();
        let now = now_secs().unwrap();
        let license = sign_license(
            LicenseClaims {
                protocol_version: PROTOCOL_VERSION,
                product: PRODUCT.to_owned(),
                license_id: hex_encode(&[1; 16]),
                device_public_key: device.clone(),
                binding_version: 3,
                issued_at: now - 1,
                offline_until: now + 3600,
            },
            &issuer,
        )
        .unwrap();
        client.validate_and_save(&license, &device).unwrap();
        (root, issuer, client, license)
    }

    fn respond_once(
        status: u16,
        extra_headers: &str,
        body: &[u8],
    ) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let response = format!(
            "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n",
            body.len()
        );
        let body = body.to_vec();
        let handle = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error)
                        if error.kind() == io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("test HTTP accept failed: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(stream);
            let mut length = 0;
            loop {
                let mut line = String::new();
                assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse::<usize>().unwrap();
                }
            }
            assert!(length < 8192);
            let mut request_body = vec![0; length];
            reader.read_exact(&mut request_body).unwrap();
            let mut stream = reader.into_inner();
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(&body).unwrap();
            String::from_utf8(request_body).unwrap()
        });
        (endpoint, handle)
    }

    #[test]
    fn concurrent_device_initialization_keeps_one_dpapi_identity() {
        let root = tempfile::tempdir().unwrap();
        let issuer = public_key(&generate_device_key().unwrap()).unwrap();
        let client = LicenseClient::new(root.path(), issuer.clone()).unwrap();
        let barrier = Arc::new(Barrier::new(4));
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let client = client.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    client.device_public_key().unwrap()
                })
            })
            .collect();
        let keys: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert!(keys.iter().all(|key| key == &keys[0]));
        assert_eq!(
            LicenseClient::new(root.path(), issuer)
                .unwrap()
                .device_public_key()
                .unwrap(),
            keys[0]
        );
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn invalid_or_copied_licenses_never_replace_a_valid_cache() {
        let (root, issuer, client, license) = fixture();
        let original = fs::read(root.path().join(LICENSE_FILE)).unwrap();
        let device = client.device_public_key().unwrap();
        let mut bad_signature = license.clone();
        bad_signature.signature = hex_encode(&[0; 64]);
        let mut expired = license.claims.clone();
        expired.issued_at = 1;
        expired.offline_until = 2;
        let wrong_issuer =
            sign_license(license.claims.clone(), &generate_device_key().unwrap()).unwrap();
        let mut other_device = license.claims.clone();
        other_device.device_public_key = public_key(&generate_device_key().unwrap()).unwrap();
        for invalid in [
            bad_signature,
            sign_license(expired, &issuer).unwrap(),
            wrong_issuer,
            sign_license(other_device, &issuer).unwrap(),
        ] {
            assert!(client.validate_and_save(&invalid, &device).is_err());
            assert_eq!(fs::read(root.path().join(LICENSE_FILE)).unwrap(), original);
        }
        let copy = tempfile::tempdir().unwrap();
        fs::write(copy.path().join(LICENSE_FILE), &original).unwrap();
        let copied_client = LicenseClient::new(copy.path(), public_key(&issuer).unwrap()).unwrap();
        assert!(copied_client.status().is_err());
        assert!(!copy.path().join(DEVICE_FILE).exists());
        copied_client.device_public_key().unwrap();
        assert!(copied_client.status().is_err());
        assert_eq!(client.status().unwrap(), license);
    }

    #[test]
    fn missing_or_corrupt_identity_is_not_regenerated_by_status_or_refresh() {
        let (root, _issuer, client, _license) = fixture();
        let path = root.path().join(DEVICE_FILE);
        fs::remove_file(&path).unwrap();
        assert!(client.status().is_err());
        assert!(client.refresh("http://127.0.0.1:1").is_err());
        assert!(!path.exists());
        fs::write(&path, b"corrupt-dpapi-data").unwrap();
        assert!(client.device_public_key().is_err());
        assert!(client.status().is_err());
        assert_eq!(fs::read(path).unwrap(), b"corrupt-dpapi-data");
    }

    #[test]
    fn malformed_and_oversized_cached_licenses_fail_without_mutating_identity() {
        let (root, _issuer, client, _license) = fixture();
        let key = fs::read(root.path().join(DEVICE_FILE)).unwrap();
        for body in [b"{bad json".to_vec(), vec![b' '; LICENSE_LIMIT + 1]] {
            fs::write(root.path().join(LICENSE_FILE), body).unwrap();
            assert!(client.status().is_err());
            assert_eq!(fs::read(root.path().join(DEVICE_FILE)).unwrap(), key);
        }
    }

    #[test]
    fn network_failure_preserves_offline_license() {
        let (root, _issuer, client, license) = fixture();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let previous = fs::read(root.path().join(LICENSE_FILE)).unwrap();
        assert!(client.refresh(&endpoint).is_err());
        assert_eq!(fs::read(root.path().join(LICENSE_FILE)).unwrap(), previous);
        assert_eq!(client.status().unwrap(), license);
    }

    #[test]
    fn redirects_are_not_followed_and_activation_codes_stay_at_the_original_origin() {
        let (_root, _issuer, client, _license) = fixture();
        let redirected = TcpListener::bind("127.0.0.1:0").unwrap();
        redirected.set_nonblocking(true).unwrap();
        let location = format!(
            "Location: http://{}/stolen\r\n",
            redirected.local_addr().unwrap()
        );
        let (endpoint, server) = respond_once(307, &location, b"");
        let code = hex_encode(&[9; 32]);
        let error = client.activate(&endpoint, &code).unwrap_err().to_string();
        assert!(error.contains("HTTP 307"), "{error}");
        assert!(!error.contains(&code));
        assert!(server.join().unwrap().contains(&code));
        assert_eq!(
            redirected.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn bad_http_responses_and_server_errors_preserve_cache_and_bound_diagnostics() {
        let (root, _issuer, client, _license) = fixture();
        let previous = fs::read(root.path().join(LICENSE_FILE)).unwrap();
        let code = hex_encode(&[5; 32]);
        for (status, body, expected) in [
            (200, b"{malformed".to_vec(), "key must be a string"),
            (200, vec![b' '; LICENSE_LIMIT + 1], "too large"),
            (
                409,
                br#"{"code":"bound_elsewhere"}"#.to_vec(),
                "HTTP 409 (bound_elsewhere)",
            ),
            (
                500,
                br#"{"code":"\u001b[31msecret"}"#.to_vec(),
                "HTTP 500 (server_rejected_request)",
            ),
        ] {
            let (endpoint, server) = respond_once(status, "", &body);
            let error = client.activate(&endpoint, &code).unwrap_err().to_string();
            server.join().unwrap();
            assert!(error.contains(expected), "{error}");
            assert!(!error.contains('\x1b'));
            assert_eq!(fs::read(root.path().join(LICENSE_FILE)).unwrap(), previous);
        }
    }

    #[test]
    fn refresh_checks_response_identity_and_version_and_signs_each_request() {
        let (_root, issuer, client, license) = fixture();
        for change in 0..3 {
            let mut claims = license.claims.clone();
            match change {
                0 => claims.license_id = hex_encode(&[2; 16]),
                1 => claims.binding_version -= 1,
                _ => claims.binding_version += 1,
            }
            let signed = sign_license(claims, &issuer).unwrap();
            let (endpoint, server) = respond_once(200, "", &serde_json::to_vec(&signed).unwrap());
            let result = client.refresh(&endpoint);
            let proof: RefreshRequest = serde_json::from_str(&server.join().unwrap()).unwrap();
            verify_refresh(&proof).unwrap();
            assert_eq!(proof.license_id, license.claims.license_id);
            if change < 2 {
                assert!(result.is_err());
                assert_eq!(client.status().unwrap(), license);
            } else {
                assert_eq!(result.unwrap(), signed);
                assert_eq!(client.status().unwrap(), signed);
            }
        }
    }

    #[test]
    fn interrupted_release_retries_same_receipt_and_blocks_restored_old_cache() {
        let (root, issuer, client, license) = fixture();
        let original = fs::read(root.path().join(LICENSE_FILE)).unwrap();
        let (endpoint, server) = respond_once(503, "", br#"{"code":"internal_error"}"#);
        assert!(client.release(&endpoint).is_err());
        let first: ReleaseRequest = serde_json::from_str(&server.join().unwrap()).unwrap();
        clipferry_license_protocol::verify_release(&first).unwrap();
        assert!(client.has_pending_release());
        assert!(client.status().is_err());
        let restarted = LicenseClient::new(root.path(), public_key(&issuer).unwrap()).unwrap();
        assert!(restarted.status().is_err());
        let receipt = ReleaseResponse {
            license_id: first.license_id.clone(),
            binding_version: first.binding_version,
            nonce: first.nonce.clone(),
        };
        let (endpoint, server) = respond_once(200, "", &serde_json::to_vec(&receipt).unwrap());
        restarted.release(&endpoint).unwrap();
        let retried: ReleaseRequest = serde_json::from_str(&server.join().unwrap()).unwrap();
        assert_eq!(retried.nonce, first.nonce);
        assert!(!restarted.has_pending_release());
        assert!(!restarted.has_cached_license());
        fs::write(root.path().join(LICENSE_FILE), original).unwrap();
        assert!(restarted.status().is_err());
        let mut new_claims = license.claims;
        new_claims.binding_version += 2;
        let new_license = sign_license(new_claims, &issuer).unwrap();
        restarted
            .validate_and_save(&new_license, &restarted.device_public_key().unwrap())
            .unwrap();
        assert!(restarted.status().is_ok());
    }

    #[test]
    fn server_revocation_and_clock_rollback_fail_closed_without_changing_system_time() {
        let (root, _issuer, client, _license) = fixture();
        let clock = root.path().join("clock.dpapi");
        fs::write(
            &clock,
            protect_for_current_user(&serde_json::to_vec(&(now_secs().unwrap() + 3600)).unwrap())
                .unwrap(),
        )
        .unwrap();
        assert!(
            client
                .status()
                .unwrap_err()
                .to_string()
                .contains("clock moved")
        );
        fs::remove_file(clock).unwrap();
        assert!(client.status().is_ok());
        let (endpoint, server) = respond_once(403, "", br#"{"code":"device_mismatch"}"#);
        assert!(client.refresh(&endpoint).is_err());
        server.join().unwrap();
        assert!(client.status().is_err());
    }

    #[test]
    fn failed_atomic_replacement_removes_only_its_own_temporary_file() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join(LICENSE_FILE);
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("keep"), b"keep").unwrap();
        assert!(write_replace_atomic(&destination, b"new").is_err());
        assert_eq!(fs::read(destination.join("keep")).unwrap(), b"keep");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn server_origin_validation_rejects_credential_leaks() {
        assert_eq!(
            server_url("http://127.0.0.1:8787", "activate").unwrap(),
            "http://127.0.0.1:8787/v1/activate"
        );
        assert_eq!(
            server_url("https://license.example.com", "refresh").unwrap(),
            "https://license.example.com/v1/refresh"
        );
        assert!(server_url("http://localhost:8787@evil.example", "activate").is_err());
        assert!(server_url("http://evil.example", "activate").is_err());
        assert!(server_url("https://license.example.com/path", "activate").is_err());
    }
}
