//! Small, independently deployable licensing service. It binds to loopback only.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::{
    Json, Router, extract::DefaultBodyLimit, extract::State, http::StatusCode, routing::post,
};
use clipferry_license_protocol::{
    ActivateRequest, ErrorResponse, LicenseClaims, PRODUCT, PROTOCOL_VERSION, RefreshRequest,
    ReleaseRequest, ReleaseResponse, SignedLicense, code_hash, generate_code, hex_decode,
    hex_encode, public_key, sign_license, verify_refresh, verify_release,
};
use ring::rand::{SecureRandom, SystemRandom};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};

const OFFLINE_SECS: u64 = 30 * 24 * 60 * 60;
type RateWindow = (u64, u32, HashMap<String, u32>);

#[derive(Clone)]
pub struct LicenseService {
    connection: Arc<Mutex<Connection>>,
    signing_key: Arc<Vec<u8>>,
    rate: Arc<Mutex<RateWindow>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceError {
    InvalidRequest,
    UnknownCode,
    BoundElsewhere,
    UnknownLicense,
    DeviceMismatch,
    ReplayedNonce,
    Internal,
}

impl ServiceError {
    fn response(self) -> (StatusCode, Json<ErrorResponse>) {
        let (status, code) = match self {
            Self::InvalidRequest => (StatusCode::BAD_REQUEST, "invalid_request"),
            Self::UnknownCode => (StatusCode::NOT_FOUND, "unknown_code"),
            Self::BoundElsewhere => (StatusCode::CONFLICT, "bound_elsewhere"),
            Self::UnknownLicense => (StatusCode::NOT_FOUND, "unknown_license"),
            Self::DeviceMismatch => (StatusCode::FORBIDDEN, "device_mismatch"),
            Self::ReplayedNonce => (StatusCode::CONFLICT, "replayed_nonce"),
            Self::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
        };
        (
            status,
            Json(ErrorResponse {
                code: code.to_owned(),
            }),
        )
    }
}

impl LicenseService {
    /// Opens a service database and verifies its schema. The key is never stored in SQLite.
    pub fn open(path: &Path, signing_key: Vec<u8>) -> Result<Self, String> {
        let issuer = public_key(&signing_key).map_err(str::to_owned)?;
        let mut connection = Connection::open(path).map_err(|error| error.to_string())?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|error| error.to_string())?;
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = FULL;
                 CREATE TABLE IF NOT EXISTS service_metadata (
                   name TEXT PRIMARY KEY,
                   value TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS licenses (
                   id TEXT PRIMARY KEY,
                   code_hash BLOB NOT NULL UNIQUE,
                   device_key TEXT,
                   binding_version INTEGER NOT NULL DEFAULT 0,
                   disabled INTEGER NOT NULL DEFAULT 0,
                   created_at INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS refresh_nonces (
                   license_id TEXT NOT NULL REFERENCES licenses(id),
                   nonce TEXT NOT NULL,
                   used_at INTEGER NOT NULL,
                   PRIMARY KEY (license_id, nonce)
                 );
                 CREATE TABLE IF NOT EXISTS release_receipts (
                   license_id TEXT NOT NULL, nonce TEXT NOT NULL, device_key TEXT NOT NULL,
                   binding_version INTEGER NOT NULL, PRIMARY KEY (license_id, nonce)
                 );",
            )
            .map_err(|error| error.to_string())?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        transaction.execute(
            "INSERT OR IGNORE INTO service_metadata (name, value) VALUES ('issuer_public_key', ?1)",
            [&issuer],
        ).map_err(|error| error.to_string())?;
        let stored_issuer: String = transaction
            .query_row(
                "SELECT value FROM service_metadata WHERE name = 'issuer_public_key'",
                [],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if stored_issuer != issuer {
            return Err("signing key does not match this license database".to_owned());
        }
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            signing_key: Arc::new(signing_key),
            rate: Arc::new(Mutex::new((0, 0, HashMap::new()))),
        })
    }

    /// Creates independent, 256-bit single-device codes. Return codes only once to the operator.
    pub fn create_codes(&self, count: usize) -> Result<Vec<String>, String> {
        if !(1..=100).contains(&count) {
            return Err("count must be between 1 and 100".to_owned());
        }
        let now = now_secs().map_err(|error| format!("clock error: {error:?}"))?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| "database lock poisoned")?;
        let transaction = connection
            .transaction()
            .map_err(|error| error.to_string())?;
        let mut codes = Vec::with_capacity(count);
        for _ in 0..count {
            let code = generate_code().map_err(str::to_owned)?;
            let mut identifier = [0; 16];
            SystemRandom::new()
                .fill(&mut identifier)
                .map_err(|_| "random generation failed")?;
            transaction
                .execute(
                    "INSERT INTO licenses (id, code_hash, created_at) VALUES (?1, ?2, ?3)",
                    params![
                        hex_encode(&identifier),
                        code_hash(&code).unwrap().as_slice(),
                        now
                    ],
                )
                .map_err(|error| error.to_string())?;
            codes.push(code);
        }
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(codes)
    }

    pub fn activate(&self, request: &ActivateRequest) -> Result<SignedLicense, ServiceError> {
        let code_hash =
            code_hash(&request.activation_code).map_err(|_| ServiceError::InvalidRequest)?;
        let _ = hex_decode::<32>(&request.device_public_key)
            .map_err(|_| ServiceError::InvalidRequest)?;
        let now = now_secs()?;
        let mut connection = self.connection.lock().map_err(|_| ServiceError::Internal)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| ServiceError::Internal)?;
        let row: Option<(String, Option<String>, u64, bool)> = transaction
            .query_row(
                "SELECT id, device_key, binding_version, disabled FROM licenses WHERE code_hash = ?1",
                [code_hash.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(|_| ServiceError::Internal)?;
        let (id, bound_device, old_version, disabled) = row.ok_or(ServiceError::UnknownCode)?;
        if disabled {
            return Err(ServiceError::UnknownCode);
        }
        if bound_device
            .as_deref()
            .is_some_and(|key| key != request.device_public_key)
        {
            return Err(ServiceError::BoundElsewhere);
        }
        let version = if bound_device.is_none() {
            transaction
                .execute(
                    "UPDATE licenses SET device_key = ?1, binding_version = binding_version + 1 WHERE id = ?2",
                    params![request.device_public_key, id],
                )
                .map_err(|_| ServiceError::Internal)?;
            old_version + 1
        } else {
            old_version
        };
        transaction.commit().map_err(|_| ServiceError::Internal)?;
        self.issue(&id, &request.device_public_key, version, now)
    }

    pub fn refresh(&self, request: &RefreshRequest) -> Result<SignedLicense, ServiceError> {
        verify_refresh(request).map_err(|_| ServiceError::InvalidRequest)?;
        let now = now_secs()?;
        let mut connection = self.connection.lock().map_err(|_| ServiceError::Internal)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| ServiceError::Internal)?;
        let row: Option<(Option<String>, u64, bool)> = transaction
            .query_row(
                "SELECT device_key, binding_version, disabled FROM licenses WHERE id = ?1",
                [&request.license_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(|_| ServiceError::Internal)?;
        let (bound_device, version, disabled) = row.ok_or(ServiceError::UnknownLicense)?;
        if disabled || bound_device.as_deref() != Some(&request.device_public_key) {
            return Err(ServiceError::DeviceMismatch);
        }
        let inserted = transaction
            .execute(
                "INSERT OR IGNORE INTO refresh_nonces (license_id, nonce, used_at) VALUES (?1, ?2, ?3)",
                params![request.license_id, request.nonce, now],
            )
            .map_err(|_| ServiceError::Internal)?;
        if inserted != 1 {
            return Err(ServiceError::ReplayedNonce);
        }
        transaction.commit().map_err(|_| ServiceError::Internal)?;
        self.issue(
            &request.license_id,
            &request.device_public_key,
            version,
            now,
        )
    }

    /// Releases only the binding signed by its current device. Retrying a receipt is harmless.
    pub fn release(&self, request: &ReleaseRequest) -> Result<ReleaseResponse, ServiceError> {
        verify_release(request).map_err(|_| ServiceError::InvalidRequest)?;
        if now_secs()?.abs_diff(request.timestamp) > 300 {
            return Err(ServiceError::InvalidRequest);
        }
        let mut connection = self.connection.lock().map_err(|_| ServiceError::Internal)?;
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| ServiceError::Internal)?;
        let receipt: Option<(String,u64)> = tx.query_row(
            "SELECT device_key, binding_version FROM release_receipts WHERE license_id=?1 AND nonce=?2",
            params![request.license_id, request.nonce], |row| Ok((row.get(0)?,row.get(1)?))
        ).optional().map_err(|_| ServiceError::Internal)?;
        if let Some((device, version)) = receipt {
            if device != request.device_public_key || version != request.binding_version {
                return Err(ServiceError::InvalidRequest);
            }
        } else {
            let changed = tx.execute(
                "UPDATE licenses SET device_key=NULL, binding_version=binding_version+1 WHERE id=?1 AND device_key=?2 AND binding_version=?3 AND disabled=0",
                params![request.license_id, request.device_public_key, request.binding_version]
            ).map_err(|_| ServiceError::Internal)?;
            if changed != 1 {
                return Err(ServiceError::DeviceMismatch);
            }
            tx.execute(
                "INSERT INTO release_receipts VALUES (?1,?2,?3,?4)",
                params![
                    request.license_id,
                    request.nonce,
                    request.device_public_key,
                    request.binding_version
                ],
            )
            .map_err(|_| ServiceError::Internal)?;
        }
        tx.commit().map_err(|_| ServiceError::Internal)?;
        Ok(ReleaseResponse {
            license_id: request.license_id.clone(),
            binding_version: request.binding_version,
            nonce: request.nonce.clone(),
        })
    }

    fn issue(
        &self,
        id: &str,
        device: &str,
        version: u64,
        now: u64,
    ) -> Result<SignedLicense, ServiceError> {
        let claims = LicenseClaims {
            protocol_version: PROTOCOL_VERSION,
            product: PRODUCT.to_owned(),
            license_id: id.to_owned(),
            device_public_key: device.to_owned(),
            binding_version: version,
            issued_at: now,
            offline_until: now
                .checked_add(OFFLINE_SECS)
                .ok_or(ServiceError::Internal)?,
        };
        sign_license(claims, &self.signing_key).map_err(|_| ServiceError::Internal)
    }

    pub fn unbind(&self, id: &str) -> Result<bool, String> {
        let _ = hex_decode::<16>(id).map_err(str::to_owned)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| "database lock poisoned")?;
        Ok(connection
            .execute(
                "UPDATE licenses SET device_key = NULL, binding_version = binding_version + 1 WHERE id = ?1 AND disabled = 0",
                [id],
            )
            .map_err(|error| error.to_string())?
            == 1)
    }

    /// Permanently stops online activation and refresh for an issued code.
    /// Existing offline licenses remain valid until their signed expiry.
    pub fn disable(&self, id: &str) -> Result<bool, String> {
        hex_decode::<16>(id).map_err(str::to_owned)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| "database lock poisoned")?;
        Ok(connection.execute(
            "UPDATE licenses SET disabled = 1, device_key = NULL, binding_version = binding_version + 1 WHERE id = ?1 AND disabled = 0",
            [id],
        ).map_err(|error| error.to_string())? == 1)
    }

    pub fn license_id_for_code(&self, code: &str) -> Result<Option<String>, String> {
        let hash = code_hash(code).map_err(str::to_owned)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| "database lock poisoned")?;
        connection
            .query_row(
                "SELECT id FROM licenses WHERE code_hash = ?1",
                [hash.as_slice()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())
    }

    pub fn router(self) -> Router {
        Router::new()
            .route("/health", axum::routing::get(|| async { "ok" }))
            .route("/v1/activate", post(activate_handler))
            .route("/v1/refresh", post(refresh_handler))
            .route("/v1/release", post(release_handler))
            .layer(DefaultBodyLimit::max(8 * 1024))
            .layer(axum::middleware::from_fn_with_state(
                self.clone(),
                rate_limit,
            ))
            .with_state(self)
    }
}

async fn rate_limit(
    State(service): State<LicenseService>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    // Only the loopback tunnel may reach this service. Never trust this header on a public listener.
    let peer = request
        .headers()
        .get("cf-connecting-ip")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<std::net::IpAddr>().ok())
        .map_or_else(|| "loopback".to_owned(), |v| v.to_string());
    let allowed = service.rate.lock().is_ok_and(|mut rate| {
        let minute = now_secs().unwrap_or(0) / 60;
        if rate.0 != minute {
            *rate = (minute, 0, HashMap::new());
        }
        if rate.1 >= 600 || (rate.2.len() >= 4096 && !rate.2.contains_key(&peer)) {
            return false;
        }
        rate.1 += 1;
        let count = rate.2.entry(peer).or_default();
        *count += 1;
        *count <= 30
    });
    if !allowed {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("retry-after", "60")],
            Json(ErrorResponse {
                code: "rate_limited".to_owned(),
            }),
        )
            .into_response();
    }
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        "cache-control",
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}

async fn release_handler(
    State(service): State<LicenseService>,
    Json(request): Json<ReleaseRequest>,
) -> Result<Json<ReleaseResponse>, (StatusCode, Json<ErrorResponse>)> {
    tokio::task::spawn_blocking(move || service.release(&request))
        .await
        .map_err(|_| ServiceError::Internal.response())?
        .map(Json)
        .map_err(ServiceError::response)
}

fn now_secs() -> Result<u64, ServiceError> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ServiceError::Internal)?
        .as_secs())
}

async fn activate_handler(
    State(service): State<LicenseService>,
    Json(request): Json<ActivateRequest>,
) -> Result<Json<SignedLicense>, (StatusCode, Json<ErrorResponse>)> {
    tokio::task::spawn_blocking(move || service.activate(&request))
        .await
        .map_err(|_| ServiceError::Internal.response())?
        .map(Json)
        .map_err(ServiceError::response)
}

async fn refresh_handler(
    State(service): State<LicenseService>,
    Json(request): Json<RefreshRequest>,
) -> Result<Json<SignedLicense>, (StatusCode, Json<ErrorResponse>)> {
    tokio::task::spawn_blocking(move || service.refresh(&request))
        .await
        .map_err(|_| ServiceError::Internal.response())?
        .map(Json)
        .map_err(ServiceError::response)
}

pub fn key_fingerprint(key: &[u8]) -> String {
    hex_encode(&Sha256::digest(key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipferry_license_protocol::{public_key, sign_refresh, verify_license};
    use std::sync::{Arc, Barrier};

    fn fixture() -> (tempfile::TempDir, Vec<u8>, LicenseService) {
        let directory = tempfile::tempdir().unwrap();
        let key = clipferry_license_protocol::generate_device_key().unwrap();
        let service =
            LicenseService::open(&directory.path().join("licenses.sqlite"), key.clone()).unwrap();
        (directory, key, service)
    }

    fn refresh_request(license: &SignedLicense, device: &[u8], marker: u8) -> RefreshRequest {
        let id = &license.claims.license_id;
        let nonce = hex_encode(&[marker; 16]);
        RefreshRequest {
            license_id: id.clone(),
            device_public_key: public_key(device).unwrap(),
            signature: sign_refresh(id, &nonce, device).unwrap(),
            nonce,
        }
    }

    #[test]
    fn bindings_and_replay_history_survive_database_reopen() {
        let (directory, issuer, service) = fixture();
        let code = service.create_codes(1).unwrap().remove(0);
        let device = clipferry_license_protocol::generate_device_key().unwrap();
        let request = ActivateRequest {
            activation_code: code,
            device_public_key: public_key(&device).unwrap(),
        };
        let license = service.activate(&request).unwrap();
        let refresh = refresh_request(&license, &device, 10);
        service.refresh(&refresh).unwrap();
        drop(service);
        let reopened =
            LicenseService::open(&directory.path().join("licenses.sqlite"), issuer).unwrap();
        assert_eq!(
            reopened.activate(&request).unwrap().claims.binding_version,
            1
        );
        assert_eq!(
            reopened.refresh(&refresh).unwrap_err(),
            ServiceError::ReplayedNonce
        );
        let other = ActivateRequest {
            device_public_key: hex_encode(&[42; 32]),
            ..request
        };
        assert_eq!(
            reopened.activate(&other).unwrap_err(),
            ServiceError::BoundElsewhere
        );
        assert!(
            reopened
                .refresh(&refresh_request(&license, &device, 11))
                .is_ok()
        );
    }

    #[test]
    fn changing_the_issuer_cannot_silently_resign_an_existing_database() {
        let (directory, issuer, service) = fixture();
        let code = service.create_codes(1).unwrap().remove(0);
        let id = service.license_id_for_code(&code).unwrap();
        drop(service);
        let wrong_key = clipferry_license_protocol::generate_device_key().unwrap();
        assert!(
            LicenseService::open(&directory.path().join("licenses.sqlite"), wrong_key).is_err()
        );
        let original =
            LicenseService::open(&directory.path().join("licenses.sqlite"), issuer).unwrap();
        assert_eq!(original.license_id_for_code(&code).unwrap(), id);
    }

    #[test]
    fn disabled_codes_cannot_activate_refresh_or_be_unbound() {
        let (_directory, _issuer, service) = fixture();
        let code = service.create_codes(1).unwrap().remove(0);
        let device = clipferry_license_protocol::generate_device_key().unwrap();
        let request = ActivateRequest {
            activation_code: code,
            device_public_key: public_key(&device).unwrap(),
        };
        let license = service.activate(&request).unwrap();
        assert!(service.disable(&license.claims.license_id).unwrap());
        assert!(!service.disable(&license.claims.license_id).unwrap());
        assert!(!service.unbind(&license.claims.license_id).unwrap());
        assert_eq!(
            service.activate(&request).unwrap_err(),
            ServiceError::UnknownCode
        );
        assert_eq!(
            service
                .refresh(&refresh_request(&license, &device, 20))
                .unwrap_err(),
            ServiceError::DeviceMismatch
        );
    }

    #[test]
    fn invalid_requests_do_not_consume_an_unbound_code() {
        let (_directory, _issuer, service) = fixture();
        let code = service.create_codes(1).unwrap().remove(0);
        let request = ActivateRequest {
            activation_code: code.clone(),
            device_public_key: "invalid".to_owned(),
        };
        assert_eq!(
            service.activate(&request).unwrap_err(),
            ServiceError::InvalidRequest
        );
        let device = clipferry_license_protocol::generate_device_key().unwrap();
        let request = ActivateRequest {
            device_public_key: public_key(&device).unwrap(),
            ..request
        };
        let unknown = ActivateRequest {
            activation_code: generate_code().unwrap(),
            ..request.clone()
        };
        assert_eq!(
            service.activate(&unknown).unwrap_err(),
            ServiceError::UnknownCode
        );
        let license = service.activate(&request).unwrap();
        assert_eq!(license.claims.binding_version, 1);
        let attacker = clipferry_license_protocol::generate_device_key().unwrap();
        assert_eq!(
            service
                .refresh(&refresh_request(&license, &attacker, 30))
                .unwrap_err(),
            ServiceError::DeviceMismatch
        );
        let mut forged = refresh_request(&license, &device, 30);
        forged.signature = hex_encode(&[0; 64]);
        assert_eq!(
            service.refresh(&forged).unwrap_err(),
            ServiceError::InvalidRequest
        );
        assert!(
            service
                .refresh(&refresh_request(&license, &device, 30))
                .is_ok()
        );
    }

    #[test]
    fn code_batches_are_unique_hashed_and_transactional() {
        let (_directory, _issuer, service) = fixture();
        assert!(service.create_codes(0).is_err());
        assert!(service.create_codes(101).is_err());
        let codes = service.create_codes(2).unwrap();
        assert_ne!(codes[0], codes[1]);
        {
            let connection = service.connection.lock().unwrap();
            let hash: Vec<u8> = connection
                .query_row(
                    "SELECT code_hash FROM licenses WHERE id = ?1",
                    [service_id(&connection, &codes[0])],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(hash, code_hash(&codes[0]).unwrap());
            connection.execute_batch("CREATE TRIGGER reject_partial_batch BEFORE INSERT ON licenses WHEN (SELECT COUNT(*) FROM licenses) >= 3 BEGIN SELECT RAISE(ABORT, 'test failure'); END;").unwrap();
        }
        assert!(service.create_codes(2).is_err());
        let connection = service.connection.lock().unwrap();
        let count: usize = connection
            .query_row("SELECT COUNT(*) FROM licenses", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            count, 2,
            "failed batches must not leave the first inserted code behind"
        );
    }

    #[test]
    fn device_release_is_authenticated_idempotent_and_cannot_release_a_new_binding() {
        use clipferry_license_protocol::{generate_device_key, sign_release};
        let (_directory, _issuer, service) = fixture();
        let device = generate_device_key().unwrap();
        let other = generate_device_key().unwrap();
        let code = service.create_codes(1).unwrap().remove(0);
        let activation = ActivateRequest {
            activation_code: code,
            device_public_key: public_key(&device).unwrap(),
        };
        let license = service.activate(&activation).unwrap();
        assert_eq!(
            license.claims.offline_until - license.claims.issued_at,
            OFFLINE_SECS
        );
        let mut request = ReleaseRequest {
            license_id: license.claims.license_id.clone(),
            device_public_key: activation.device_public_key.clone(),
            binding_version: license.claims.binding_version,
            timestamp: now_secs().unwrap(),
            nonce: hex_encode(&[15; 16]),
            signature: String::new(),
        };
        sign_release(&mut request, &device).unwrap();
        let mut forged = request.clone();
        forged.binding_version += 1;
        assert_eq!(
            service.release(&forged).unwrap_err(),
            ServiceError::InvalidRequest
        );
        let mut wrong_device = request.clone();
        wrong_device.device_public_key = public_key(&other).unwrap();
        sign_release(&mut wrong_device, &other).unwrap();
        assert_eq!(
            service.release(&wrong_device).unwrap_err(),
            ServiceError::DeviceMismatch
        );
        let mut expired = request.clone();
        expired.timestamp -= 301;
        sign_release(&mut expired, &device).unwrap();
        assert_eq!(
            service.release(&expired).unwrap_err(),
            ServiceError::InvalidRequest
        );
        let receipt = service.release(&request).unwrap();
        assert_eq!(service.release(&request).unwrap(), receipt);
        let new_activation = ActivateRequest {
            device_public_key: public_key(&other).unwrap(),
            ..activation.clone()
        };
        let new_license = service.activate(&new_activation).unwrap();
        assert!(new_license.claims.binding_version > license.claims.binding_version);
        assert_eq!(service.release(&request).unwrap(), receipt);
        assert!(
            service
                .refresh(&refresh_request(&new_license, &other, 31))
                .is_ok()
        );
        request.nonce = hex_encode(&[16; 16]);
        sign_release(&mut request, &device).unwrap();
        assert_eq!(
            service.release(&request).unwrap_err(),
            ServiceError::DeviceMismatch
        );
        assert_eq!(
            service.activate(&activation).unwrap_err(),
            ServiceError::BoundElsewhere
        );
    }

    fn service_id(connection: &Connection, code: &str) -> String {
        connection
            .query_row(
                "SELECT id FROM licenses WHERE code_hash = ?1",
                [code_hash(code).unwrap().as_slice()],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    fn separate_database_connections_cannot_double_bind_a_code() {
        let (directory, issuer, first) = fixture();
        let second =
            LicenseService::open(&directory.path().join("licenses.sqlite"), issuer).unwrap();
        let code = first.create_codes(1).unwrap().remove(0);
        let barrier = Arc::new(Barrier::new(2));
        let handles: Vec<_> = [first, second]
            .into_iter()
            .map(|service| {
                let barrier = Arc::clone(&barrier);
                let code = code.clone();
                std::thread::spawn(move || {
                    let key = clipferry_license_protocol::generate_device_key().unwrap();
                    let request = ActivateRequest {
                        activation_code: code,
                        device_public_key: public_key(&key).unwrap(),
                    };
                    barrier.wait();
                    service.activate(&request)
                })
            })
            .collect();
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|value| value.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|value| value.as_ref().err() == Some(&ServiceError::BoundElsewhere))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn http_rejects_invalid_json_oversize_bodies_and_reused_codes() {
        use axum::body::{Body, to_bytes};
        use axum::http::Request;
        use tower::ServiceExt as _;
        let (_directory, issuer, service) = fixture();
        let code = service.create_codes(1).unwrap().remove(0);
        let device =
            public_key(&clipferry_license_protocol::generate_device_key().unwrap()).unwrap();
        let router = service.router();
        for (body, content_type, expected) in [
            ("{}".to_owned(), "application/json", StatusCode::UNPROCESSABLE_ENTITY),
            ("{".to_owned(), "application/json", StatusCode::BAD_REQUEST),
            (" ".repeat(8193), "application/json", StatusCode::PAYLOAD_TOO_LARGE),
            ("{}".to_owned(), "text/plain", StatusCode::UNSUPPORTED_MEDIA_TYPE),
            (serde_json::json!({"activation_code": code, "device_public_key": device, "extra": true}).to_string(), "application/json", StatusCode::UNPROCESSABLE_ENTITY),
        ] {
            let response = router.clone().oneshot(Request::builder().method("POST").uri("/v1/activate").header("content-type", content_type).body(Body::from(body)).unwrap()).await.unwrap();
            assert_eq!(response.status(), expected);
        }
        let activate = |key: &str| {
            Request::builder()
                .method("POST")
                .uri("/v1/activate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"activation_code":code,"device_public_key":key}).to_string(),
                ))
                .unwrap()
        };
        let response = router.clone().oneshot(activate(&device)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 8192).await.unwrap();
        let license: SignedLicense = serde_json::from_slice(&body).unwrap();
        verify_license(
            &license,
            &public_key(&issuer).unwrap(),
            &device,
            now_secs().unwrap(),
        )
        .unwrap();
        let response = router
            .oneshot(activate(&hex_encode(&[7; 32])))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let error: ErrorResponse =
            serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
        assert_eq!(error.code, "bound_elsewhere");
    }

    #[test]
    fn codes_bind_once_and_refresh_proofs_cannot_replay() {
        let directory = tempfile::tempdir().unwrap();
        let signing_key = clipferry_license_protocol::generate_device_key().unwrap();
        let service = LicenseService::open(
            &directory.path().join("licenses.sqlite"),
            signing_key.clone(),
        )
        .unwrap();
        let code = service.create_codes(1).unwrap().remove(0);
        let first = clipferry_license_protocol::generate_device_key().unwrap();
        let second = clipferry_license_protocol::generate_device_key().unwrap();
        let first_public = public_key(&first).unwrap();
        let first_request = ActivateRequest {
            activation_code: code.clone(),
            device_public_key: first_public.clone(),
        };
        let license = service.activate(&first_request).unwrap();
        assert_eq!(license.claims.binding_version, 1);
        assert!(
            verify_license(
                &license,
                &public_key(&signing_key).unwrap(),
                &first_public,
                now_secs().unwrap()
            )
            .is_ok()
        );
        assert_eq!(
            service
                .activate(&first_request)
                .unwrap()
                .claims
                .binding_version,
            1
        );
        let other_request = ActivateRequest {
            activation_code: code.clone(),
            device_public_key: public_key(&second).unwrap(),
        };
        assert_eq!(
            service.activate(&other_request).unwrap_err(),
            ServiceError::BoundElsewhere
        );
        let nonce = hex_encode(&[7; 16]);
        let refresh = RefreshRequest {
            license_id: license.claims.license_id.clone(),
            device_public_key: first_public,
            signature: sign_refresh(&license.claims.license_id, &nonce, &first).unwrap(),
            nonce,
        };
        assert!(service.refresh(&refresh).is_ok());
        assert_eq!(
            service.refresh(&refresh).unwrap_err(),
            ServiceError::ReplayedNonce
        );
        assert!(service.unbind(&license.claims.license_id).unwrap());
        assert_eq!(
            service
                .refresh(&RefreshRequest {
                    nonce: hex_encode(&[8; 16]),
                    signature: sign_refresh(
                        &license.claims.license_id,
                        &hex_encode(&[8; 16]),
                        &first
                    )
                    .unwrap(),
                    ..refresh
                })
                .unwrap_err(),
            ServiceError::DeviceMismatch
        );
        assert_eq!(
            service
                .activate(&other_request)
                .unwrap()
                .claims
                .binding_version,
            3
        );
    }

    #[test]
    fn simultaneous_activations_cannot_bind_two_devices() {
        let directory = tempfile::tempdir().unwrap();
        let signing_key = clipferry_license_protocol::generate_device_key().unwrap();
        let service =
            LicenseService::open(&directory.path().join("licenses.sqlite"), signing_key).unwrap();
        let code = service.create_codes(1).unwrap().remove(0);
        let barrier = Arc::new(Barrier::new(3));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let service = service.clone();
                let barrier = Arc::clone(&barrier);
                let code = code.clone();
                std::thread::spawn(move || {
                    let key = clipferry_license_protocol::generate_device_key().unwrap();
                    let request = ActivateRequest {
                        activation_code: code,
                        device_public_key: public_key(&key).unwrap(),
                    };
                    barrier.wait();
                    service.activate(&request)
                })
            })
            .collect();
        barrier.wait();
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| result.as_ref().err() == Some(&ServiceError::BoundElsewhere))
                .count(),
            1
        );
    }
}
