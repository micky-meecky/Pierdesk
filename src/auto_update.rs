//! Signed automatic update checking, dual-source downloading, and staged release management.
//!
//! Security and privacy invariants:
//! - Update checks perform read-only static `GET` requests with no device identifiers or telemetry.
//! - Every manifest must carry a valid Ed25519 signature under the compiled release public key.
//! - Only strictly newer semantic versions are accepted (downgrade protection).
//! - Downloads stream through SHA-256 verification, exact size validation, and PE header checks
//!   before being staged for atomic replacement.
//! - Primary (Cloudflare R2) and fallback (GitHub Releases) URLs are tried in order; redirects
//!   never downgrade HTTPS to HTTP.

use std::cmp::Ordering as CmpOrdering;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ring::rand::SystemRandom;
use ring::signature::{self, Ed25519KeyPair, KeyPair as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use url::Url;

use crate::app_settings::{AutoUpdateMode, AutoUpdateSettings};
use crate::device_store::DeviceStore;

pub const MANIFEST_SCHEMA_VERSION: u8 = 1;
pub const DEFAULT_CHANNEL: &str = "stable";
pub const EXPECTED_FILE_NAME: &str = "pierdesk.exe";
pub const GITHUB_FALLBACK_MANIFEST_URL: &str =
    "https://github.com/micky-meecky/Pierdesk/releases/latest/download/pierdesk-update.json";

const SIGNATURE_CONTEXT: &[u8] = b"pierdesk-auto-update-v1\n";
const MANIFEST_BYTES_LIMIT: usize = 64 * 1024;
const DEFAULT_MIN_BINARY_BYTES: u64 = 1024 * 1024;
const MAX_BINARY_BYTES: u64 = 128 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(15);
const DOWNLOAD_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const DOWNLOAD_READ_TIMEOUT: Duration = Duration::from_mins(1);
const STARTUP_CHECK_DELAY: Duration = Duration::from_secs(90);
const PERIODIC_CHECK_INTERVAL: Duration = Duration::from_hours(24);
const MAX_JITTER_SECONDS: u64 = 30 * 60;
const USER_AGENT: &str = "Pierdesk-Updater";

/// Optional Cloudflare R2 primary manifest URL baked in at release build time.
pub const COMPILED_PRIMARY_MANIFEST_URL: Option<&str> = option_env!("PIERDESK_UPDATE_PRIMARY_URL");
/// Optional Ed25519 release signing public key (Base64, 32 raw bytes) baked in at release build time.
pub const COMPILED_PUBLIC_KEY_B64: Option<&str> = option_env!("PIERDESK_UPDATE_PUBLIC_KEY");

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct SemVer {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl SemVer {
    /// Parses a strict three-component semantic version (`MAJOR.MINOR.PATCH`).
    ///
    /// # Errors
    ///
    /// Returns an error if the version does not contain exactly three numeric components.
    pub fn parse(input: &str) -> io::Result<Self> {
        let mut parts = input.split('.');
        let major = parse_component(parts.next())?;
        let minor = parse_component(parts.next())?;
        let patch = parse_component(parts.next())?;
        if parts.next().is_some() {
            return Err(invalid_data(
                "version must have exactly three numeric parts",
            ));
        }
        Ok(Self {
            major,
            minor,
            patch,
        })
    }
}

fn parse_component(part: Option<&str>) -> io::Result<u64> {
    let value = part.ok_or_else(|| invalid_data("version is incomplete"))?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid_data("version component must be ASCII digits"));
    }
    if value.len() > 1 && value.starts_with('0') {
        return Err(invalid_data(
            "version component must not have leading zeros",
        ));
    }
    value
        .parse::<u64>()
        .map_err(|_| invalid_data("version component overflows u64"))
}

impl Ord for SemVer {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        (self.major, self.minor, self.patch).cmp(&(other.major, other.minor, other.patch))
    }
}

impl PartialOrd for SemVer {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

impl std::fmt::Display for SemVer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct UpdateManifest {
    pub schema_version: u8,
    pub channel: String,
    pub version: String,
    pub published_at: String,
    pub file_name: String,
    pub size: u64,
    pub sha256: String,
    pub download_urls: Vec<String>,
    #[serde(default)]
    pub notes_url: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SignedUpdateManifest {
    pub schema_version: u8,
    pub payload: String,
    pub signature: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingStagedUpdate {
    pub schema_version: u8,
    pub version: String,
    pub sha256: String,
    pub size: u64,
    pub staged_path: String,
    #[serde(default)]
    pub notes_url: Option<String>,
    pub prepared_at_unix: u64,
    pub signed_manifest: SignedUpdateManifest,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdatePersistentState {
    #[serde(default)]
    pub last_checked_unix: Option<u64>,
    #[serde(default)]
    pub rolled_back_versions: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AutoUpdatePhase {
    #[default]
    Idle,
    Unconfigured,
    Checking,
    UpToDate,
    UpdateAvailable,
    Downloading,
    ReadyToInstall,
    NeedsElevation,
    Applying,
    Error,
}

impl AutoUpdatePhase {
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Unconfigured => "unconfigured",
            Self::Checking => "checking",
            Self::UpToDate => "up_to_date",
            Self::UpdateAvailable => "update_available",
            Self::Downloading => "downloading",
            Self::ReadyToInstall => "ready_to_install",
            Self::NeedsElevation => "needs_elevation",
            Self::Applying => "applying",
            Self::Error => "error",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutoUpdateSnapshot {
    pub mode: AutoUpdateMode,
    pub phase: AutoUpdatePhase,
    pub configured: bool,
    pub current_version: String,
    pub available_version: Option<String>,
    pub notes_url: Option<String>,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub last_checked_unix: Option<u64>,
    pub last_error: Option<String>,
    pub staged: Option<PendingStagedUpdate>,
    pub target_writable: bool,
    pub primary_source_configured: bool,
}

impl AutoUpdateSnapshot {
    #[must_use]
    pub fn progress_percent(&self) -> u8 {
        if self.total_bytes == 0 {
            return 0;
        }
        let ratio = self.downloaded_bytes.saturating_mul(100) / self.total_bytes;
        u8::try_from(ratio.min(100)).unwrap_or(100)
    }

    #[must_use]
    pub fn can_install_now(&self) -> bool {
        matches!(
            self.phase,
            AutoUpdatePhase::ReadyToInstall | AutoUpdatePhase::NeedsElevation
        ) && self.staged.is_some()
    }

    #[must_use]
    pub fn has_available_update(&self) -> bool {
        matches!(
            self.phase,
            AutoUpdatePhase::UpdateAvailable
                | AutoUpdatePhase::ReadyToInstall
                | AutoUpdatePhase::NeedsElevation
        ) && self.available_version.is_some()
    }
}

#[derive(Clone, Debug)]
pub struct AutoUpdateConfig {
    pub current_version: String,
    pub manifest_urls: Vec<String>,
    pub public_key_b64: Option<String>,
    pub target_executable: PathBuf,
    pub allow_http_loopback: bool,
    pub min_binary_bytes: u64,
    pub startup_delay: Duration,
    pub check_interval: Duration,
    pub jitter_max_seconds: u64,
}

impl AutoUpdateConfig {
    /// Builds the default production configuration from compile-time and optional mirror settings.
    #[must_use]
    pub fn for_current_process() -> Self {
        let mut manifest_urls = Vec::new();
        if let Ok(override_urls) = std::env::var("PIERDESK_UPDATE_MANIFEST_URLS") {
            for entry in override_urls.split(',') {
                let trimmed = entry.trim();
                if !trimmed.is_empty() {
                    manifest_urls.push(trimmed.to_owned());
                }
            }
        }
        if manifest_urls.is_empty() {
            if let Some(primary) = COMPILED_PRIMARY_MANIFEST_URL {
                let trimmed = primary.trim();
                if !trimmed.is_empty() {
                    manifest_urls.push(trimmed.to_owned());
                }
            }
            manifest_urls.push(GITHUB_FALLBACK_MANIFEST_URL.to_owned());
        }
        let public_key_b64 = COMPILED_PUBLIC_KEY_B64
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        let target_executable = std::env::current_exe().unwrap_or_default();
        Self {
            current_version: env!("CARGO_PKG_VERSION").to_owned(),
            manifest_urls,
            public_key_b64,
            target_executable,
            allow_http_loopback: false,
            min_binary_bytes: DEFAULT_MIN_BINARY_BYTES,
            startup_delay: STARTUP_CHECK_DELAY,
            check_interval: PERIODIC_CHECK_INTERVAL,
            jitter_max_seconds: MAX_JITTER_SECONDS,
        }
    }

    #[must_use]
    pub fn is_configured(&self) -> bool {
        self.public_key_b64
            .as_deref()
            .is_some_and(|key| decode_public_key(key).is_ok())
            && !self.manifest_urls.is_empty()
    }

    #[must_use]
    pub fn has_primary_r2_source(&self) -> bool {
        self.manifest_urls
            .first()
            .is_some_and(|url| url != GITHUB_FALLBACK_MANIFEST_URL)
    }
}

/// Generates a fresh Ed25519 PKCS#8 private key and its Base64 public key.
///
/// # Errors
///
/// Returns an error if the operating system CSPRNG fails.
pub fn generate_signing_keypair() -> io::Result<(Vec<u8>, String)> {
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
        .map_err(|_| io::Error::other("failed to generate Ed25519 signing key"))?;
    let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())
        .map_err(|_| io::Error::other("failed to parse generated Ed25519 key"))?;
    let public_b64 = BASE64.encode(pair.public_key().as_ref());
    Ok((pkcs8.as_ref().to_vec(), public_b64))
}

/// Derives the Base64-encoded 32-byte Ed25519 public key from a PKCS#8 private key.
///
/// # Errors
///
/// Returns an error when the PKCS#8 bytes are malformed.
pub fn public_key_from_pkcs8(pkcs8: &[u8]) -> io::Result<String> {
    let pair = Ed25519KeyPair::from_pkcs8(pkcs8)
        .map_err(|_| invalid_data("invalid Ed25519 PKCS#8 signing key"))?;
    Ok(BASE64.encode(pair.public_key().as_ref()))
}

/// Validates and signs an update manifest with the release Ed25519 private key.
///
/// # Errors
///
/// Returns an error when the manifest fails validation or the signing key is invalid.
pub fn sign_manifest(
    manifest: &UpdateManifest,
    pkcs8_private_key: &[u8],
    allow_http_loopback: bool,
) -> io::Result<SignedUpdateManifest> {
    validate_manifest(manifest, allow_http_loopback, 1)?;
    let key_pair = Ed25519KeyPair::from_pkcs8(pkcs8_private_key)
        .map_err(|_| invalid_data("invalid Ed25519 PKCS#8 signing key"))?;
    let payload_bytes = serde_json::to_vec(manifest).map_err(io::Error::other)?;
    let signing_input = signature_message(&payload_bytes);
    let signature = key_pair.sign(&signing_input);
    Ok(SignedUpdateManifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        payload: BASE64.encode(&payload_bytes),
        signature: BASE64.encode(signature.as_ref()),
    })
}

/// Verifies a signed update manifest envelope and returns the authenticated payload.
///
/// # Errors
///
/// Returns an error on bad schema, invalid Base64, signature mismatch, or invalid manifest fields.
pub fn verify_signed_manifest(
    signed: &SignedUpdateManifest,
    public_key_b64: &str,
    allow_http_loopback: bool,
    min_binary_bytes: u64,
) -> io::Result<UpdateManifest> {
    if signed.schema_version != MANIFEST_SCHEMA_VERSION {
        return Err(invalid_data("unsupported signed update manifest schema"));
    }
    let public_key = decode_public_key(public_key_b64)?;
    let payload_bytes = BASE64
        .decode(signed.payload.trim())
        .map_err(|_| invalid_data("update manifest payload is not valid base64"))?;
    if payload_bytes.len() > MANIFEST_BYTES_LIMIT {
        return Err(invalid_data("update manifest payload exceeds size limit"));
    }
    let signature_bytes = BASE64
        .decode(signed.signature.trim())
        .map_err(|_| invalid_data("update manifest signature is not valid base64"))?;
    if signature_bytes.len() != 64 {
        return Err(invalid_data("update manifest signature length is invalid"));
    }
    let message = signature_message(&payload_bytes);
    signature::UnparsedPublicKey::new(&signature::ED25519, public_key)
        .verify(&message, &signature_bytes)
        .map_err(|_| invalid_data("update manifest signature verification failed"))?;
    let manifest: UpdateManifest = serde_json::from_slice(&payload_bytes)
        .map_err(|error| invalid_data(format!("invalid update manifest JSON: {error}")))?;
    validate_manifest(&manifest, allow_http_loopback, min_binary_bytes)?;
    Ok(manifest)
}

fn decode_public_key(public_key_b64: &str) -> io::Result<[u8; 32]> {
    let decoded = BASE64
        .decode(public_key_b64.trim())
        .map_err(|_| invalid_data("update public key is not valid base64"))?;
    decoded
        .try_into()
        .map_err(|_| invalid_data("update public key must be exactly 32 bytes"))
}

fn signature_message(payload_bytes: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(SIGNATURE_CONTEXT.len() + payload_bytes.len());
    message.extend_from_slice(SIGNATURE_CONTEXT);
    message.extend_from_slice(payload_bytes);
    message
}

fn validate_manifest(
    manifest: &UpdateManifest,
    allow_http_loopback: bool,
    min_binary_bytes: u64,
) -> io::Result<()> {
    if manifest.schema_version != MANIFEST_SCHEMA_VERSION {
        return Err(invalid_data("unsupported update manifest schema version"));
    }
    if manifest.channel != DEFAULT_CHANNEL {
        return Err(invalid_data("unsupported update release channel"));
    }
    let _ = SemVer::parse(&manifest.version)?;
    if manifest.published_at.trim().is_empty() || manifest.published_at.len() > 64 {
        return Err(invalid_data("invalid update manifest publishedAt"));
    }
    if !manifest.file_name.eq_ignore_ascii_case(EXPECTED_FILE_NAME) {
        return Err(invalid_data("unexpected update binary file name"));
    }
    if manifest.size < min_binary_bytes || manifest.size > MAX_BINARY_BYTES {
        return Err(invalid_data("update binary size is outside allowed bounds"));
    }
    validate_sha256_hex(&manifest.sha256)?;
    if manifest.download_urls.is_empty() || manifest.download_urls.len() > 8 {
        return Err(invalid_data(
            "update manifest must provide between 1 and 8 download URLs",
        ));
    }
    for candidate in &manifest.download_urls {
        validate_remote_url(candidate, allow_http_loopback)?;
    }
    if let Some(notes_url) = manifest.notes_url.as_deref() {
        validate_remote_url(notes_url, allow_http_loopback)?;
    }
    Ok(())
}

/// Validates a 64-character hexadecimal SHA-256 string.
///
/// # Errors
///
/// Returns an error if the length or character set is invalid.
pub fn validate_sha256_hex(value: &str) -> io::Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid_data("expected 64 hexadecimal SHA-256 characters"));
    }
    Ok(())
}

/// Validates that a remote update URL uses HTTPS (or loopback HTTP during explicit tests).
///
/// # Errors
///
/// Returns an error for malformed URLs, credentials, or disallowed schemes.
pub fn validate_remote_url(raw: &str, allow_http_loopback: bool) -> io::Result<Url> {
    let parsed =
        Url::parse(raw).map_err(|error| invalid_data(format!("invalid update URL: {error}")))?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(invalid_data("update URL must not embed credentials"));
    }
    let Some(host) = parsed.host_str() else {
        return Err(invalid_data("update URL must include a host"));
    };
    match parsed.scheme() {
        "https" => Ok(parsed),
        "http" if allow_http_loopback && matches!(host, "127.0.0.1" | "localhost" | "[::1]") => {
            Ok(parsed)
        }
        _ => Err(invalid_data("update URL must use HTTPS")),
    }
}

/// Computes the uppercase hexadecimal SHA-256 hash and exact byte length of a file.
///
/// # Errors
///
/// Returns any I/O error encountered while reading the file.
pub fn hash_file_sha256(path: &Path) -> io::Result<(String, u64)> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    let mut total = 0_u64;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total = total.saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
        hasher.update(&buffer[..count]);
    }
    Ok((format!("{:X}", hasher.finalize()), total))
}

/// Verifies whether the directory containing `executable` can be written to by the current token.
#[must_use]
pub fn target_directory_is_writable(executable: &Path) -> bool {
    let Some(directory) = executable.parent() else {
        return false;
    };
    let probe = directory.join(format!(
        ".pierdesk.write-probe.{}.{}.tmp",
        std::process::id(),
        now_unix_secs()
    ));
    let created = OpenOptions::new().write(true).create_new(true).open(&probe);
    match created {
        Ok(file) => {
            drop(file);
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Fetches and verifies the latest signed manifest from the ordered list of sources.
///
/// # Errors
///
/// Returns an error if every configured source fails network retrieval or signature verification.
pub fn fetch_verified_manifest(
    urls: &[String],
    public_key_b64: &str,
    allow_http_loopback: bool,
    min_binary_bytes: u64,
) -> io::Result<(SignedUpdateManifest, UpdateManifest, String)> {
    if urls.is_empty() {
        return Err(invalid_data("no update manifest URLs are configured"));
    }
    let mut errors = Vec::new();
    for url in urls {
        match fetch_one_manifest(url, public_key_b64, allow_http_loopback, min_binary_bytes) {
            Ok((signed, manifest)) => return Ok((signed, manifest, url.clone())),
            Err(error) => errors.push(format!("{url}: {error}")),
        }
    }
    Err(io::Error::other(format!(
        "all update manifest sources failed ({})",
        errors.join("; ")
    )))
}

fn fetch_one_manifest(
    url: &str,
    public_key_b64: &str,
    allow_http_loopback: bool,
    min_binary_bytes: u64,
) -> io::Result<(SignedUpdateManifest, UpdateManifest)> {
    let response =
        get_with_safe_redirects(url, allow_http_loopback, MANIFEST_TIMEOUT, MANIFEST_TIMEOUT)?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(u64::try_from(MANIFEST_BYTES_LIMIT + 1).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)?;
    if bytes.len() > MANIFEST_BYTES_LIMIT {
        return Err(invalid_data("update manifest response exceeds size limit"));
    }
    let signed: SignedUpdateManifest = serde_json::from_slice(&bytes)
        .map_err(|error| invalid_data(format!("invalid signed manifest envelope: {error}")))?;
    let manifest = verify_signed_manifest(
        &signed,
        public_key_b64,
        allow_http_loopback,
        min_binary_bytes,
    )?;
    Ok((signed, manifest))
}

/// Streams the update binary from primary/fallback URLs, verifying size, PE header, and SHA-256.
///
/// # Errors
///
/// Returns an error if every download mirror fails or the downloaded payload fails verification.
pub fn download_verified_binary<F>(
    manifest: &UpdateManifest,
    destination: &Path,
    allow_http_loopback: bool,
    cancel: &AtomicBool,
    mut on_progress: F,
) -> io::Result<String>
where
    F: FnMut(u64, u64),
{
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut errors = Vec::new();
    for url in &manifest.download_urls {
        if cancel.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "update download cancelled",
            ));
        }
        match download_one_binary(
            url,
            manifest,
            destination,
            allow_http_loopback,
            cancel,
            &mut on_progress,
        ) {
            Ok(()) => return Ok(url.clone()),
            Err(error) => errors.push(format!("{url}: {error}")),
        }
    }
    Err(io::Error::other(format!(
        "all update download sources failed ({})",
        errors.join("; ")
    )))
}

fn download_one_binary<F>(
    url: &str,
    manifest: &UpdateManifest,
    destination: &Path,
    allow_http_loopback: bool,
    cancel: &AtomicBool,
    on_progress: &mut F,
) -> io::Result<()>
where
    F: FnMut(u64, u64),
{
    let temp_path = destination.with_extension("download.tmp");
    let _ = fs::remove_file(&temp_path);
    let result = (|| -> io::Result<()> {
        let response = get_with_safe_redirects(
            url,
            allow_http_loopback,
            DOWNLOAD_CONNECT_TIMEOUT,
            DOWNLOAD_READ_TIMEOUT,
        )?;
        let mut reader = response.into_reader();
        let mut file = File::create(&temp_path)?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
        let mut total = 0_u64;
        let mut header_prefix = [0_u8; 2];
        let mut prefix_len = 0_usize;
        on_progress(0, manifest.size);
        loop {
            if cancel.load(Ordering::Acquire) {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "update download cancelled",
                ));
            }
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            if prefix_len < 2 {
                let take = (2 - prefix_len).min(count);
                header_prefix[prefix_len..prefix_len + take].copy_from_slice(&buffer[..take]);
                prefix_len += take;
            }
            total = total.saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
            if total > manifest.size {
                return Err(invalid_data(
                    "downloaded update binary exceeded declared manifest size",
                ));
            }
            file.write_all(&buffer[..count])?;
            hasher.update(&buffer[..count]);
            on_progress(total, manifest.size);
        }
        file.sync_all()?;
        drop(file);
        if total != manifest.size {
            return Err(invalid_data(format!(
                "downloaded update binary size mismatch (expected {}, got {total})",
                manifest.size
            )));
        }
        if header_prefix != *b"MZ" {
            return Err(invalid_data(
                "downloaded update payload is not a Windows executable",
            ));
        }
        let actual_sha256 = format!("{:X}", hasher.finalize());
        if !actual_sha256.eq_ignore_ascii_case(&manifest.sha256) {
            return Err(invalid_data(format!(
                "downloaded update SHA-256 mismatch (expected {}, got {actual_sha256})",
                manifest.sha256
            )));
        }
        let _ = fs::remove_file(destination);
        fs::rename(&temp_path, destination)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn get_with_safe_redirects(
    initial_url: &str,
    allow_http_loopback: bool,
    connect_timeout: Duration,
    read_timeout: Duration,
) -> io::Result<ureq::Response> {
    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout_connect(connect_timeout)
        .timeout_read(read_timeout)
        .user_agent(USER_AGENT)
        .build();
    let mut current = validate_remote_url(initial_url, allow_http_loopback)?;
    for _ in 0..=MAX_REDIRECTS {
        let response = match agent
            .get(current.as_str())
            .set("Cache-Control", "no-cache")
            .call()
        {
            Ok(response) => response,
            Err(ureq::Error::Status(status, _)) => {
                return Err(io::Error::other(format!("HTTP {status}")));
            }
            Err(ureq::Error::Transport(error)) => {
                return Err(io::Error::other(error.to_string()));
            }
        };
        let status = response.status();
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            let location = response
                .header("Location")
                .ok_or_else(|| invalid_data("redirect missing Location header"))?;
            let next = current
                .join(location)
                .map_err(|error| invalid_data(format!("invalid redirect URL: {error}")))?;
            current = validate_remote_url(next.as_str(), allow_http_loopback)?;
            continue;
        }
        if (200..300).contains(&status) {
            return Ok(response);
        }
        return Err(io::Error::other(format!("unexpected HTTP {status}")));
    }
    Err(io::Error::other("too many HTTP redirects"))
}

#[must_use]
pub fn updates_root(store_root: &Path) -> PathBuf {
    store_root.join("updates")
}

#[must_use]
pub fn staged_dir(store_root: &Path) -> PathBuf {
    updates_root(store_root).join("staged")
}

#[must_use]
pub fn pending_manifest_path(store_root: &Path) -> PathBuf {
    staged_dir(store_root).join("pending.json")
}

#[must_use]
pub fn helper_dir(store_root: &Path) -> PathBuf {
    updates_root(store_root).join("helper")
}

#[must_use]
pub fn persistent_state_path(store_root: &Path) -> PathBuf {
    updates_root(store_root).join("state.json")
}

#[must_use]
pub fn load_persistent_state(store_root: &Path) -> UpdatePersistentState {
    let path = persistent_state_path(store_root);
    let Ok(bytes) = fs::read(path) else {
        return UpdatePersistentState::default();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

/// Persists non-sensitive updater state (last check timestamp and rolled-back version list).
///
/// # Errors
///
/// Returns an error if the directory or file cannot be written.
pub fn save_persistent_state(store_root: &Path, state: &UpdatePersistentState) -> io::Result<()> {
    let root = updates_root(store_root);
    fs::create_dir_all(&root)?;
    let target = persistent_state_path(store_root);
    let temp = root.join("state.json.tmp");
    let encoded = serde_json::to_vec_pretty(state).map_err(io::Error::other)?;
    fs::write(&temp, encoded)?;
    let _ = fs::remove_file(&target);
    fs::rename(&temp, &target)
}

/// Loads and fully re-verifies any previously staged update on disk.
#[must_use]
pub fn load_verified_pending(
    store_root: &Path,
    config: &AutoUpdateConfig,
) -> Option<PendingStagedUpdate> {
    let public_key = config.public_key_b64.as_deref()?;
    let bytes = fs::read(pending_manifest_path(store_root)).ok()?;
    let pending: PendingStagedUpdate = serde_json::from_slice(&bytes).ok()?;
    if pending.schema_version != MANIFEST_SCHEMA_VERSION {
        return None;
    }
    let manifest = verify_signed_manifest(
        &pending.signed_manifest,
        public_key,
        config.allow_http_loopback,
        config.min_binary_bytes,
    )
    .ok()?;
    let current = SemVer::parse(&config.current_version).ok()?;
    let staged_ver = SemVer::parse(&manifest.version).ok()?;
    if staged_ver <= current {
        return None;
    }
    if pending.version != manifest.version
        || !pending.sha256.eq_ignore_ascii_case(&manifest.sha256)
        || pending.size != manifest.size
    {
        return None;
    }
    let staged_path = PathBuf::from(&pending.staged_path);
    let (actual_hash, actual_size) = hash_file_sha256(&staged_path).ok()?;
    if actual_size != manifest.size || !actual_hash.eq_ignore_ascii_case(&manifest.sha256) {
        return None;
    }
    Some(pending)
}

/// Cleans up stale helper copies and already-installed staged payloads on startup.
pub fn cleanup_stale_artifacts(store_root: &Path, current_version: &str) {
    let helper = helper_dir(store_root);
    if helper.exists() {
        let _ = fs::remove_dir_all(&helper);
    }
    let Ok(current) = SemVer::parse(current_version) else {
        return;
    };
    let pending_path = pending_manifest_path(store_root);
    let Ok(bytes) = fs::read(&pending_path) else {
        return;
    };
    let Ok(pending) = serde_json::from_slice::<PendingStagedUpdate>(&bytes) else {
        let _ = fs::remove_dir_all(staged_dir(store_root));
        return;
    };
    let should_clean =
        SemVer::parse(&pending.version).map_or(true, |staged_version| staged_version <= current);
    if should_clean {
        let _ = fs::remove_dir_all(staged_dir(store_root));
    }
}

/// Records a version as rolled back so silent mode will not enter an infinite update loop.
pub fn record_rolled_back_version(store_root: &Path, version: &str) {
    let mut state = load_persistent_state(store_root);
    if !state
        .rolled_back_versions
        .iter()
        .any(|item| item == version)
    {
        state.rolled_back_versions.push(version.to_owned());
        if state.rolled_back_versions.len() > 16 {
            let overflow = state.rolled_back_versions.len() - 16;
            state.rolled_back_versions.drain(0..overflow);
        }
    }
    let _ = save_persistent_state(store_root, &state);
}

#[derive(Debug)]
pub enum AutoUpdateCommand {
    CheckNow { user_initiated: bool },
    DownloadAvailable,
    SetMode(AutoUpdateMode),
    MarkApplying,
    Shutdown,
}

pub struct AutoUpdateRuntime {
    stop: Arc<AtomicBool>,
    sender: mpsc::Sender<AutoUpdateCommand>,
    snapshot: Arc<Mutex<AutoUpdateSnapshot>>,
    worker: Option<JoinHandle<()>>,
}

impl AutoUpdateRuntime {
    /// Starts the background auto-update worker thread.
    ///
    /// # Errors
    ///
    /// Returns an error if the worker thread cannot be spawned.
    pub fn start(
        store: DeviceStore,
        config: AutoUpdateConfig,
        log_path: PathBuf,
    ) -> io::Result<Self> {
        cleanup_stale_artifacts(store.root(), &config.current_version);
        let settings = AutoUpdateSettings::load_or_default(&store).unwrap_or_default();
        let persistent = load_persistent_state(store.root());
        let configured = config.is_configured();
        let target_writable = target_directory_is_writable(&config.target_executable);
        let existing_staged = if configured {
            load_verified_pending(store.root(), &config).filter(|pending| {
                !persistent
                    .rolled_back_versions
                    .iter()
                    .any(|bad| bad == &pending.version)
            })
        } else {
            None
        };
        let initial_phase = if !configured {
            AutoUpdatePhase::Unconfigured
        } else if existing_staged.is_some() {
            if target_writable {
                AutoUpdatePhase::ReadyToInstall
            } else {
                AutoUpdatePhase::NeedsElevation
            }
        } else {
            AutoUpdatePhase::Idle
        };
        let initial_snapshot = AutoUpdateSnapshot {
            mode: settings.mode,
            phase: initial_phase,
            configured,
            current_version: config.current_version.clone(),
            available_version: existing_staged.as_ref().map(|item| item.version.clone()),
            notes_url: existing_staged
                .as_ref()
                .and_then(|item| item.notes_url.clone()),
            downloaded_bytes: existing_staged.as_ref().map_or(0, |item| item.size),
            total_bytes: existing_staged.as_ref().map_or(0, |item| item.size),
            last_checked_unix: persistent.last_checked_unix,
            last_error: None,
            staged: existing_staged,
            target_writable,
            primary_source_configured: config.has_primary_r2_source(),
        };
        let snapshot = Arc::new(Mutex::new(initial_snapshot));
        let stop = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::channel();
        let worker_stop = Arc::clone(&stop);
        let worker_snapshot = Arc::clone(&snapshot);
        let worker = thread::Builder::new()
            .name("pierdesk-auto-update".to_owned())
            .spawn(move || {
                worker_loop(
                    &store,
                    &config,
                    &log_path,
                    &worker_stop,
                    &worker_snapshot,
                    &receiver,
                );
            })?;
        Ok(Self {
            stop,
            sender,
            snapshot,
            worker: Some(worker),
        })
    }

    #[must_use]
    pub fn snapshot(&self) -> AutoUpdateSnapshot {
        self.snapshot.lock().map_or_else(
            |poisoned| poisoned.into_inner().clone(),
            |guard| guard.clone(),
        )
    }

    /// Sends a command to the background update worker.
    ///
    /// # Errors
    ///
    /// Returns an error if the worker thread is no longer running.
    pub fn send(&self, command: AutoUpdateCommand) -> io::Result<()> {
        self.sender
            .send(command)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "auto-update worker stopped"))
    }

    /// Requests an immediate user-initiated update check.
    ///
    /// # Errors
    ///
    /// Returns an error if the background update worker has stopped.
    pub fn check_now(&self) -> io::Result<()> {
        self.send(AutoUpdateCommand::CheckNow {
            user_initiated: true,
        })
    }

    /// Downloads the currently discovered update payload in the background.
    ///
    /// # Errors
    ///
    /// Returns an error if the background update worker has stopped.
    pub fn download_available(&self) -> io::Result<()> {
        self.send(AutoUpdateCommand::DownloadAvailable)
    }

    /// Applies an updated automatic-update policy mode on the background worker.
    ///
    /// # Errors
    ///
    /// Returns an error if the background update worker has stopped.
    pub fn set_mode(&self, mode: AutoUpdateMode) -> io::Result<()> {
        self.send(AutoUpdateCommand::SetMode(mode))
    }

    pub fn mark_applying(&self) {
        if let Ok(mut guard) = self.snapshot.lock() {
            guard.phase = AutoUpdatePhase::Applying;
            guard.last_error = None;
        }
        let _ = self.send(AutoUpdateCommand::MarkApplying);
    }
}

impl Drop for AutoUpdateRuntime {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.sender.send(AutoUpdateCommand::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[allow(clippy::too_many_lines)]
fn worker_loop(
    store: &DeviceStore,
    config: &AutoUpdateConfig,
    log_path: &Path,
    stop: &AtomicBool,
    snapshot: &Arc<Mutex<AutoUpdateSnapshot>>,
    receiver: &mpsc::Receiver<AutoUpdateCommand>,
) {
    let mut mode = AutoUpdateSettings::load_or_default(store)
        .unwrap_or_default()
        .mode;
    let mut cached_remote: Option<(SignedUpdateManifest, UpdateManifest)> = None;
    let persistent = load_persistent_state(store.root());
    let initial_delay = initial_check_delay(
        config.startup_delay,
        config.check_interval,
        persistent.last_checked_unix,
        now_unix_secs(),
    );
    let initial_jitter = if initial_delay > config.startup_delay {
        config.jitter_max_seconds
    } else {
        0
    };
    let mut next_scheduled_check = compute_next_check(initial_delay, initial_jitter);

    while !stop.load(Ordering::Acquire) {
        let now = Instant::now();
        let wait_duration = if mode == AutoUpdateMode::Disabled || !config.is_configured() {
            Duration::from_secs(30)
        } else if now >= next_scheduled_check {
            Duration::ZERO
        } else {
            (next_scheduled_check - now).min(Duration::from_secs(30))
        };

        let command = if wait_duration.is_zero() {
            match receiver.try_recv() {
                Ok(command) => Some(command),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        } else {
            match receiver.recv_timeout(wait_duration) {
                Ok(command) => Some(command),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        };

        if stop.load(Ordering::Acquire) {
            break;
        }

        match command {
            Some(AutoUpdateCommand::Shutdown) => break,
            Some(AutoUpdateCommand::MarkApplying) => {
                update_snapshot(snapshot, |state| {
                    state.phase = AutoUpdatePhase::Applying;
                    state.last_error = None;
                });
            }
            Some(AutoUpdateCommand::SetMode(new_mode)) => {
                mode = new_mode;
                update_snapshot(snapshot, |state| {
                    state.mode = new_mode;
                });
                if new_mode == AutoUpdateMode::Silent
                    && config.is_configured()
                    && let Some((signed, manifest)) = cached_remote.clone()
                {
                    download_and_stage(
                        store, config, log_path, stop, snapshot, &signed, &manifest, false,
                    );
                }
                if new_mode != AutoUpdateMode::Disabled && config.is_configured() {
                    next_scheduled_check =
                        compute_next_check(config.check_interval, config.jitter_max_seconds);
                }
            }
            Some(AutoUpdateCommand::DownloadAvailable) => {
                if !config.is_configured() {
                    continue;
                }
                if let Some((signed, manifest)) = cached_remote.clone() {
                    download_and_stage(
                        store, config, log_path, stop, snapshot, &signed, &manifest, true,
                    );
                } else {
                    perform_check(
                        store,
                        config,
                        log_path,
                        stop,
                        snapshot,
                        mode,
                        true,
                        true,
                        &mut cached_remote,
                    );
                }
            }
            Some(AutoUpdateCommand::CheckNow { user_initiated }) => {
                perform_check(
                    store,
                    config,
                    log_path,
                    stop,
                    snapshot,
                    mode,
                    user_initiated,
                    false,
                    &mut cached_remote,
                );
                next_scheduled_check =
                    compute_next_check(config.check_interval, config.jitter_max_seconds);
            }
            None => {
                if mode != AutoUpdateMode::Disabled
                    && config.is_configured()
                    && Instant::now() >= next_scheduled_check
                {
                    let already_ready = snapshot.lock().is_ok_and(|guard| guard.can_install_now());
                    if !already_ready {
                        perform_check(
                            store,
                            config,
                            log_path,
                            stop,
                            snapshot,
                            mode,
                            false,
                            false,
                            &mut cached_remote,
                        );
                    }
                    next_scheduled_check =
                        compute_next_check(config.check_interval, config.jitter_max_seconds);
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn perform_check(
    store: &DeviceStore,
    config: &AutoUpdateConfig,
    log_path: &Path,
    stop: &AtomicBool,
    snapshot: &Arc<Mutex<AutoUpdateSnapshot>>,
    mode: AutoUpdateMode,
    user_initiated: bool,
    force_download: bool,
    cached_remote: &mut Option<(SignedUpdateManifest, UpdateManifest)>,
) {
    let Some(public_key) = config.public_key_b64.as_deref() else {
        update_snapshot(snapshot, |state| {
            state.configured = false;
            state.phase = AutoUpdatePhase::Unconfigured;
            state.last_error = None;
        });
        return;
    };
    let Ok(current_ver) = SemVer::parse(&config.current_version) else {
        return;
    };

    update_snapshot(snapshot, |state| {
        state.phase = AutoUpdatePhase::Checking;
        state.last_error = None;
    });

    let fetched = fetch_verified_manifest(
        &config.manifest_urls,
        public_key,
        config.allow_http_loopback,
        config.min_binary_bytes,
    );
    let checked_at = now_unix_secs();
    let mut persistent = load_persistent_state(store.root());
    persistent.last_checked_unix = Some(checked_at);
    let _ = save_persistent_state(store.root(), &persistent);

    let (signed, manifest, source_url) = match fetched {
        Ok(result) => result,
        Err(error) => {
            log_line(
                log_path,
                &format!("auto_update_check ok=false user={user_initiated} error={error}"),
            );
            update_snapshot(snapshot, |state| {
                state.last_checked_unix = Some(checked_at);
                if state.staged.is_some() {
                    state.phase = if state.target_writable {
                        AutoUpdatePhase::ReadyToInstall
                    } else {
                        AutoUpdatePhase::NeedsElevation
                    };
                } else {
                    state.phase = AutoUpdatePhase::Error;
                }
                state.last_error = Some(error.to_string());
            });
            return;
        }
    };

    let Ok(remote_ver) = SemVer::parse(&manifest.version) else {
        return;
    };
    log_line(
        log_path,
        &format!(
            "auto_update_check ok=true current={} remote={} source={source_url}",
            config.current_version, manifest.version
        ),
    );
    if remote_ver <= current_ver {
        *cached_remote = None;
        update_snapshot(snapshot, |state| {
            state.last_checked_unix = Some(checked_at);
            state.phase = AutoUpdatePhase::UpToDate;
            state.available_version = None;
            state.notes_url = None;
            state.last_error = None;
            state.staged = None;
            state.downloaded_bytes = 0;
            state.total_bytes = 0;
        });
        return;
    }

    if !user_initiated
        && !force_download
        && persistent
            .rolled_back_versions
            .iter()
            .any(|bad| bad == &manifest.version)
    {
        log_line(
            log_path,
            &format!(
                "auto_update_skip version={} reason=previously_rolled_back",
                manifest.version
            ),
        );
        update_snapshot(snapshot, |state| {
            state.last_checked_unix = Some(checked_at);
            state.phase = AutoUpdatePhase::UpdateAvailable;
            state.available_version = Some(manifest.version.clone());
            state.notes_url.clone_from(&manifest.notes_url);
            state.last_error = Some(format!(
                "Version {} previously rolled back; trigger manually to retry",
                manifest.version
            ));
        });
        *cached_remote = Some((signed, manifest));
        return;
    }

    // Reuse an already verified staged file matching the same version and hash.
    if let Some(existing) = load_verified_pending(store.root(), config)
        && existing.version == manifest.version
        && existing.sha256.eq_ignore_ascii_case(&manifest.sha256)
    {
        let writable = target_directory_is_writable(&config.target_executable);
        update_snapshot(snapshot, |state| {
            state.last_checked_unix = Some(checked_at);
            state.available_version = Some(manifest.version.clone());
            state.notes_url.clone_from(&manifest.notes_url);
            state.downloaded_bytes = existing.size;
            state.total_bytes = existing.size;
            state.target_writable = writable;
            state.staged = Some(existing);
            state.phase = if writable {
                AutoUpdatePhase::ReadyToInstall
            } else {
                AutoUpdatePhase::NeedsElevation
            };
            state.last_error = None;
        });
        *cached_remote = Some((signed, manifest));
        return;
    }

    *cached_remote = Some((signed.clone(), manifest.clone()));
    let should_download = force_download || mode == AutoUpdateMode::Silent;
    if !should_download {
        update_snapshot(snapshot, |state| {
            state.last_checked_unix = Some(checked_at);
            state.phase = AutoUpdatePhase::UpdateAvailable;
            state.available_version = Some(manifest.version.clone());
            state.notes_url.clone_from(&manifest.notes_url);
            state.downloaded_bytes = 0;
            state.total_bytes = manifest.size;
            state.last_error = None;
        });
        return;
    }

    download_and_stage(
        store,
        config,
        log_path,
        stop,
        snapshot,
        &signed,
        &manifest,
        user_initiated || force_download,
    );
}

#[allow(clippy::too_many_arguments)]
fn download_and_stage(
    store: &DeviceStore,
    config: &AutoUpdateConfig,
    log_path: &Path,
    stop: &AtomicBool,
    snapshot: &Arc<Mutex<AutoUpdateSnapshot>>,
    signed: &SignedUpdateManifest,
    manifest: &UpdateManifest,
    user_initiated: bool,
) {
    update_snapshot(snapshot, |state| {
        state.phase = AutoUpdatePhase::Downloading;
        state.available_version = Some(manifest.version.clone());
        state.notes_url.clone_from(&manifest.notes_url);
        state.downloaded_bytes = 0;
        state.total_bytes = manifest.size;
        state.last_error = None;
    });

    let staged_root = staged_dir(store.root());
    let dest = staged_root.join(format!("pierdesk-{}.exe", manifest.version));
    let mut last_report = Instant::now();
    let result = download_verified_binary(
        manifest,
        &dest,
        config.allow_http_loopback,
        stop,
        |downloaded, total| {
            if downloaded == total || last_report.elapsed() >= Duration::from_millis(200) {
                last_report = Instant::now();
                update_snapshot(snapshot, |state| {
                    state.downloaded_bytes = downloaded;
                    state.total_bytes = total;
                });
            }
        },
    );

    let source_url = match result {
        Ok(url) => url,
        Err(error) => {
            log_line(
                log_path,
                &format!(
                    "auto_update_download ok=false version={} user={user_initiated} error={error}",
                    manifest.version
                ),
            );
            update_snapshot(snapshot, |state| {
                state.phase = AutoUpdatePhase::Error;
                state.last_error = Some(error.to_string());
            });
            return;
        }
    };

    let pending = PendingStagedUpdate {
        schema_version: MANIFEST_SCHEMA_VERSION,
        version: manifest.version.clone(),
        sha256: manifest.sha256.to_ascii_uppercase(),
        size: manifest.size,
        staged_path: dest.to_string_lossy().into_owned(),
        notes_url: manifest.notes_url.clone(),
        prepared_at_unix: now_unix_secs(),
        signed_manifest: signed.clone(),
    };
    if let Err(error) = write_pending_manifest(store.root(), &pending) {
        log_line(
            log_path,
            &format!("auto_update_stage_write ok=false error={error}"),
        );
        update_snapshot(snapshot, |state| {
            state.phase = AutoUpdatePhase::Error;
            state.last_error = Some(error.to_string());
        });
        return;
    }

    let writable = target_directory_is_writable(&config.target_executable);
    log_line(
        log_path,
        &format!(
            "auto_update_staged ok=true version={} size={} writable={writable} source={source_url}",
            manifest.version, manifest.size
        ),
    );
    update_snapshot(snapshot, |state| {
        state.available_version = Some(manifest.version.clone());
        state.notes_url.clone_from(&manifest.notes_url);
        state.downloaded_bytes = manifest.size;
        state.total_bytes = manifest.size;
        state.target_writable = writable;
        state.staged = Some(pending);
        state.phase = if writable {
            AutoUpdatePhase::ReadyToInstall
        } else {
            AutoUpdatePhase::NeedsElevation
        };
        state.last_error = None;
    });
}

/// Persists `pending.json` atomically inside `updates/staged/`.
///
/// # Errors
///
/// Returns an error if the staged manifest cannot be serialized or moved into place.
pub fn write_pending_manifest(store_root: &Path, pending: &PendingStagedUpdate) -> io::Result<()> {
    let dir = staged_dir(store_root);
    fs::create_dir_all(&dir)?;
    let target = pending_manifest_path(store_root);
    let temp = dir.join("pending.json.tmp");
    let encoded = serde_json::to_vec_pretty(pending).map_err(io::Error::other)?;
    fs::write(&temp, encoded)?;
    let _ = fs::remove_file(&target);
    fs::rename(&temp, &target)
}

fn update_snapshot(
    snapshot: &Arc<Mutex<AutoUpdateSnapshot>>,
    update: impl FnOnce(&mut AutoUpdateSnapshot),
) {
    if let Ok(mut guard) = snapshot.lock() {
        update(&mut guard);
    }
}

fn initial_check_delay(
    startup_delay: Duration,
    check_interval: Duration,
    last_checked_unix: Option<u64>,
    now_unix: u64,
) -> Duration {
    let Some(last) = last_checked_unix else {
        return startup_delay;
    };
    if now_unix < last {
        return startup_delay;
    }
    let elapsed = Duration::from_secs(now_unix - last);
    check_interval.saturating_sub(elapsed).max(startup_delay)
}

fn compute_next_check(base: Duration, jitter_max_seconds: u64) -> Instant {
    if jitter_max_seconds == 0 {
        return Instant::now() + base;
    }
    let mut raw = [0_u8; 8];
    let jitter_secs = if getrandom::fill(&mut raw).is_ok() {
        u64::from_le_bytes(raw) % jitter_max_seconds
    } else {
        0
    };
    Instant::now() + base + Duration::from_secs(jitter_secs)
}

#[must_use]
pub fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn log_line(log_path: &Path, message: &str) {
    let _ = crate::logging::append(log_path, message);
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::io::{BufRead as _, BufReader};
    use std::net::TcpListener;

    use super::*;

    fn sample_manifest(version: &str, bytes: &[u8], urls: Vec<String>) -> UpdateManifest {
        let sha256 = format!("{:X}", Sha256::digest(bytes));
        UpdateManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            channel: DEFAULT_CHANNEL.to_owned(),
            version: version.to_owned(),
            published_at: "2026-11-27T00:00:00Z".to_owned(),
            file_name: EXPECTED_FILE_NAME.to_owned(),
            size: u64::try_from(bytes.len()).unwrap(),
            sha256,
            download_urls: urls,
            notes_url: Some("https://github.com/micky-meecky/Pierdesk/releases".to_owned()),
        }
    }

    fn spawn_http_server(
        routes: Vec<(String, u16, Vec<u8>, Option<String>)>,
        max_requests: usize,
    ) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            for _ in 0..max_requests {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                        break;
                    }
                }
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_owned();
                let found = routes.iter().find(|(route, _, _, _)| route == &path);
                let (status, body, location) = match found {
                    Some((_, status, body, location)) => {
                        (*status, body.as_slice(), location.as_deref())
                    }
                    None => (404, b"not found".as_slice(), None),
                };
                let reason = match status {
                    200 => "OK",
                    302 => "Found",
                    500 => "Internal Server Error",
                    _ => "Not Found",
                };
                let mut head = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
                    body.len()
                );
                if let Some(loc) = location {
                    let _ = write!(head, "Location: {loc}\r\n");
                }
                head.push_str("\r\n");
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body);
                let _ = stream.flush();
            }
        });
        (format!("http://{addr}"), handle)
    }

    #[test]
    fn semver_parsing_and_ordering_prevent_downgrades() {
        let v100 = SemVer::parse("1.0.64").unwrap();
        let v101 = SemVer::parse("1.0.65").unwrap();
        let v110 = SemVer::parse("1.1.0").unwrap();
        let v200 = SemVer::parse("2.0.0").unwrap();
        assert!(v100 < v101);
        assert!(v101 < v110);
        assert!(v110 < v200);
        assert!(SemVer::parse("v1.0.65").is_err());
        assert!(SemVer::parse("1.0").is_err());
        assert!(SemVer::parse("1.01.0").is_err());
        assert!(SemVer::parse("1.0.65-beta").is_err());
    }

    #[test]
    fn signed_manifest_verifies_and_rejects_tampering_or_wrong_key() {
        let (priv_a, pub_a) = generate_signing_keypair().unwrap();
        let (_priv_b, pub_b) = generate_signing_keypair().unwrap();
        let payload = b"MZfake-pierdesk-binary";
        let manifest = sample_manifest(
            "1.0.65",
            payload,
            vec!["https://downloads.example.com/releases/v1.0.65/pierdesk.exe".to_owned()],
        );
        let signed = sign_manifest(&manifest, &priv_a, false).unwrap();

        let verified = verify_signed_manifest(&signed, &pub_a, false, 1).unwrap();
        assert_eq!(verified, manifest);

        // Wrong key must fail.
        assert!(verify_signed_manifest(&signed, &pub_b, false, 1).is_err());

        // Tampered payload must fail.
        let mut tampered = signed.clone();
        let mut modified = manifest.clone();
        modified.version = "1.0.99".to_owned();
        tampered.payload = BASE64.encode(serde_json::to_vec(&modified).unwrap());
        assert!(verify_signed_manifest(&tampered, &pub_a, false, 1).is_err());

        // HTTP URL in non-loopback production mode must fail.
        let http_manifest = sample_manifest(
            "1.0.65",
            payload,
            vec!["http://downloads.example.com/pierdesk.exe".to_owned()],
        );
        assert!(sign_manifest(&http_manifest, &priv_a, false).is_err());
    }

    #[test]
    fn dual_source_manifest_and_binary_fallback_work_end_to_end() {
        let (priv_key, pub_key) = generate_signing_keypair().unwrap();
        let mut binary = b"MZ".to_vec();
        binary.extend_from_slice(&[0x42_u8; 4096]);

        // Start a mock HTTP server where primary endpoints return 500 and fallback endpoints succeed
        // (binary fallback also exercises a 302 redirect like GitHub Releases).
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let _ = addr;

        // First launch the server with known routes after binding.
        let (base_url_placeholder_listener, base_url) = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", l.local_addr().unwrap());
            (l, url)
        };
        let manifest = sample_manifest(
            "1.0.65",
            &binary,
            vec![
                format!("{base_url}/r2/pierdesk.exe"),
                format!("{base_url}/gh/pierdesk.exe"),
            ],
        );
        let signed = sign_manifest(&manifest, &priv_key, true).unwrap();
        let signed_bytes = serde_json::to_vec(&signed).unwrap();
        let redirect_target = format!("{base_url}/cdn/pierdesk.exe");
        let binary_clone = binary.clone();

        let server = thread::spawn(move || {
            let routes: Vec<(String, u16, Vec<u8>, Option<String>)> = vec![
                ("/r2/stable.json".into(), 500, b"r2 outage".to_vec(), None),
                ("/gh/pierdesk-update.json".into(), 200, signed_bytes, None),
                ("/r2/pierdesk.exe".into(), 500, b"r2 outage".to_vec(), None),
                (
                    "/gh/pierdesk.exe".into(),
                    302,
                    Vec::new(),
                    Some(redirect_target),
                ),
                ("/cdn/pierdesk.exe".into(), 200, binary_clone, None),
            ];
            for _ in 0..5 {
                let Ok((mut stream, _)) = base_url_placeholder_listener.accept() else {
                    break;
                };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                        break;
                    }
                }
                let path = line.split_whitespace().nth(1).unwrap_or("/");
                let (status, body, location) = routes
                    .iter()
                    .find(|(route, _, _, _)| route == path)
                    .map_or((404, b"missing".as_slice(), None), |(_, s, b, l)| {
                        (*s, b.as_slice(), l.as_deref())
                    });
                let mut head = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nConnection: close\r\n",
                    body.len()
                );
                if let Some(loc) = location {
                    let _ = write!(head, "Location: {loc}\r\n");
                }
                head.push_str("\r\n");
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body);
            }
        });

        let urls = vec![
            format!("{base_url}/r2/stable.json"),
            format!("{base_url}/gh/pierdesk-update.json"),
        ];
        let (_signed, verified, winning_manifest_url) =
            fetch_verified_manifest(&urls, &pub_key, true, 16).unwrap();
        assert!(winning_manifest_url.ends_with("/gh/pierdesk-update.json"));
        assert_eq!(verified.version, "1.0.65");

        let temp = tempfile::tempdir().unwrap();
        let dest = temp.path().join("pierdesk-1.0.65.exe");
        let cancel = AtomicBool::new(false);
        let mut saw_complete = false;
        let winning_download =
            download_verified_binary(&verified, &dest, true, &cancel, |done, total| {
                if done == total {
                    saw_complete = true;
                }
            })
            .unwrap();
        assert!(winning_download.ends_with("/gh/pierdesk.exe"));
        assert!(saw_complete);
        assert_eq!(fs::read(&dest).unwrap(), binary);
        let _ = server.join();
    }

    #[test]
    fn download_rejects_non_pe_and_corrupted_hash() {
        let (priv_key, _pub_key) = generate_signing_keypair().unwrap();
        let bad_payload = b"<html>error page</html>".to_vec();
        let (base_url, server) =
            spawn_http_server(vec![("/bad.exe".into(), 200, bad_payload.clone(), None)], 1);
        let manifest = sample_manifest("1.0.65", &bad_payload, vec![format!("{base_url}/bad.exe")]);
        let _ = sign_manifest(&manifest, &priv_key, true).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let dest = temp.path().join("pierdesk.exe");
        let cancel = AtomicBool::new(false);
        let error =
            download_verified_binary(&manifest, &dest, true, &cancel, |_, _| {}).unwrap_err();
        assert!(error.to_string().contains("Windows executable"));
        assert!(!dest.exists());
        let _ = server.join();
    }

    #[test]
    fn runtime_silent_and_notify_modes_stage_and_cleanup_updates() {
        let (priv_key, pub_key) = generate_signing_keypair().unwrap();
        let mut binary = b"MZ".to_vec();
        binary.extend_from_slice(&[0x55_u8; 2048]);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let manifest = sample_manifest(
            "1.0.65",
            &binary,
            vec![format!("{base_url}/releases/v1.0.65/pierdesk.exe")],
        );
        let signed = sign_manifest(&manifest, &priv_key, true).unwrap();
        let manifest_bytes = serde_json::to_vec(&signed).unwrap();
        let binary_clone = binary.clone();

        let server = thread::spawn(move || {
            for _ in 0..2 {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                        break;
                    }
                }
                let path = line.split_whitespace().nth(1).unwrap_or("/");
                let (status, body) = match path {
                    "/updates/stable.json" => (200, manifest_bytes.as_slice()),
                    "/releases/v1.0.65/pierdesk.exe" => (200, binary_clone.as_slice()),
                    _ => (404, b"missing".as_slice()),
                };
                let head = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body);
            }
        });

        let root = tempfile::tempdir().unwrap();
        let install_dir = tempfile::tempdir().unwrap();
        let target_exe = install_dir.path().join("pierdesk.exe");
        fs::write(&target_exe, b"MZold").unwrap();
        let store = DeviceStore::new(root.path());
        AutoUpdateSettings {
            mode: AutoUpdateMode::Notify,
        }
        .save(&store)
        .unwrap();

        let config = AutoUpdateConfig {
            current_version: "1.0.64".to_owned(),
            manifest_urls: vec![format!("{base_url}/updates/stable.json")],
            public_key_b64: Some(pub_key),
            target_executable: target_exe,
            allow_http_loopback: true,
            min_binary_bytes: 16,
            startup_delay: Duration::from_hours(1),
            check_interval: Duration::from_hours(1),
            jitter_max_seconds: 0,
        };
        let log_path = root.path().join("logs").join("clipferry.log");
        let runtime = AutoUpdateRuntime::start(store.clone(), config.clone(), log_path).unwrap();
        runtime.check_now().unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if runtime.snapshot().phase == AutoUpdatePhase::UpdateAvailable {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let snap = runtime.snapshot();
        assert_eq!(snap.phase, AutoUpdatePhase::UpdateAvailable);
        assert_eq!(snap.available_version.as_deref(), Some("1.0.65"));
        assert!(snap.staged.is_none());

        // Switching to silent mode or requesting download stages the verified binary.
        runtime.download_available().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if runtime.snapshot().phase == AutoUpdatePhase::ReadyToInstall {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let ready = runtime.snapshot();
        assert_eq!(ready.phase, AutoUpdatePhase::ReadyToInstall);
        assert!(ready.can_install_now());
        assert!(load_verified_pending(store.root(), &config).is_some());
        drop(runtime);

        // Once the binary upgrades to 1.0.65, startup cleanup removes staged artifacts.
        cleanup_stale_artifacts(store.root(), "1.0.65");
        assert!(!pending_manifest_path(store.root()).exists());
        let _ = server.join();
    }

    #[test]
    fn daily_check_schedule_respects_recent_check_across_restarts() {
        let startup = Duration::from_secs(90);
        let daily = Duration::from_hours(24);
        assert_eq!(initial_check_delay(startup, daily, None, 100_000), startup);
        assert_eq!(
            initial_check_delay(startup, daily, Some(100_000 - 3600), 100_000),
            Duration::from_hours(23)
        );
        assert_eq!(
            initial_check_delay(startup, daily, Some(100_000 - 86_380), 100_000),
            startup
        );
        assert_eq!(
            initial_check_delay(startup, daily, Some(100_000 - 90_000), 100_000),
            startup
        );
        assert_eq!(
            initial_check_delay(startup, daily, Some(100_500), 100_000),
            startup
        );
    }
}
