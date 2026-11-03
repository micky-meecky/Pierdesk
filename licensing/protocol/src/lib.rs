//! Shared, versioned license messages and signature verification.

use ring::rand::SystemRandom;
use ring::signature::{self, Ed25519KeyPair, KeyPair};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const PRODUCT: &str = "clipferry";
pub const PROTOCOL_VERSION: u8 = 1;
const LICENSE_CONTEXT: &[u8] = b"ClipFerry license v1\n";
const REFRESH_CONTEXT: &[u8] = b"ClipFerry refresh v1\n";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LicenseClaims {
    pub protocol_version: u8,
    pub product: String,
    pub license_id: String,
    pub device_public_key: String,
    pub binding_version: u64,
    pub issued_at: u64,
    pub offline_until: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedLicense {
    pub claims: LicenseClaims,
    pub signature: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivateRequest {
    pub activation_code: String,
    pub device_public_key: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefreshRequest {
    pub license_id: String,
    pub device_public_key: String,
    pub nonce: String,
    pub signature: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorResponse {
    pub code: String,
}

/// Device-authorized release, tied to one binding generation and a short time window.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseRequest {
    pub license_id: String,
    pub device_public_key: String,
    pub binding_version: u64,
    pub timestamp: u64,
    pub nonce: String,
    pub signature: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseResponse {
    pub license_id: String,
    pub binding_version: u64,
    pub nonce: String,
}

fn release_bytes(request: &ReleaseRequest) -> Result<Vec<u8>, &'static str> {
    hex_decode::<16>(&request.license_id)?;
    hex_decode::<32>(&request.device_public_key)?;
    hex_decode::<16>(&request.nonce)?;
    if request.binding_version == 0 {
        return Err("invalid binding version");
    }
    let mut bytes = b"ClipFerry release v1\n".to_vec();
    bytes.extend(
        serde_json::to_vec(&(
            &request.license_id,
            &request.device_public_key,
            request.binding_version,
            request.timestamp,
            &request.nonce,
        ))
        .map_err(|_| "invalid release")?,
    );
    Ok(bytes)
}

pub fn sign_release(request: &mut ReleaseRequest, key: &[u8]) -> Result<(), &'static str> {
    let key = Ed25519KeyPair::from_pkcs8(key).map_err(|_| "invalid device key")?;
    request.signature = hex_encode(key.sign(&release_bytes(request)?).as_ref());
    Ok(())
}

pub fn verify_release(request: &ReleaseRequest) -> Result<(), &'static str> {
    signature::UnparsedPublicKey::new(
        &signature::ED25519,
        hex_decode::<32>(&request.device_public_key)?,
    )
    .verify(
        &release_bytes(request)?,
        &hex_decode::<64>(&request.signature)?,
    )
    .map_err(|_| "invalid release signature")
}

pub fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[usize::from(byte >> 4)] as char);
        output.push(DIGITS[usize::from(byte & 15)] as char);
    }
    output
}

pub fn hex_decode<const N: usize>(value: &str) -> Result<[u8; N], &'static str> {
    if value.len() != N * 2 {
        return Err("invalid hex length");
    }
    let mut result = [0; N];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        result[index] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Ok(result)
}

fn nibble(byte: u8) -> Result<u8, &'static str> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err("invalid lowercase hex"),
    }
}

pub fn code_hash(code: &str) -> Result<[u8; 32], &'static str> {
    let raw = hex_decode::<32>(code)?;
    Ok(Sha256::digest(raw).into())
}

pub fn generate_code() -> Result<String, &'static str> {
    use ring::rand::SecureRandom;
    let mut raw = [0; 32];
    SystemRandom::new()
        .fill(&mut raw)
        .map_err(|_| "random generation failed")?;
    Ok(hex_encode(&raw))
}

pub fn generate_device_key() -> Result<Vec<u8>, &'static str> {
    Ok(Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
        .map_err(|_| "key generation failed")?
        .as_ref()
        .to_vec())
}

pub fn public_key(pkcs8: &[u8]) -> Result<String, &'static str> {
    let key = Ed25519KeyPair::from_pkcs8(pkcs8).map_err(|_| "invalid private key")?;
    Ok(hex_encode(key.public_key().as_ref()))
}

fn signed_claims_bytes(claims: &LicenseClaims) -> Result<Vec<u8>, &'static str> {
    let mut bytes = LICENSE_CONTEXT.to_vec();
    bytes.extend(serde_json::to_vec(claims).map_err(|_| "invalid claims")?);
    Ok(bytes)
}

pub fn sign_license(
    claims: LicenseClaims,
    signing_key: &[u8],
) -> Result<SignedLicense, &'static str> {
    validate_claims(&claims)?;
    let key = Ed25519KeyPair::from_pkcs8(signing_key).map_err(|_| "invalid signing key")?;
    let signature = hex_encode(key.sign(&signed_claims_bytes(&claims)?).as_ref());
    Ok(SignedLicense { claims, signature })
}

pub fn verify_license(
    license: &SignedLicense,
    issuer_public_key: &str,
    device_public_key: &str,
    now: u64,
) -> Result<(), &'static str> {
    verify_license_signature(license, issuer_public_key, device_public_key)?;
    if license.claims.issued_at > now || license.claims.offline_until <= now {
        return Err("license is outside its validity window");
    }
    Ok(())
}

pub fn verify_license_signature(
    license: &SignedLicense,
    issuer_public_key: &str,
    device_public_key: &str,
) -> Result<(), &'static str> {
    validate_claims(&license.claims)?;
    if license.claims.device_public_key != device_public_key {
        return Err("license belongs to another device");
    }
    let issuer = hex_decode::<32>(issuer_public_key)?;
    let signature = hex_decode::<64>(&license.signature)?;
    signature::UnparsedPublicKey::new(&signature::ED25519, issuer)
        .verify(&signed_claims_bytes(&license.claims)?, &signature)
        .map_err(|_| "invalid license signature")
}

fn validate_claims(claims: &LicenseClaims) -> Result<(), &'static str> {
    if claims.protocol_version != PROTOCOL_VERSION || claims.product != PRODUCT {
        return Err("unsupported license");
    }
    hex_decode::<16>(&claims.license_id)?;
    hex_decode::<32>(&claims.device_public_key)?;
    if claims.binding_version == 0 || claims.issued_at >= claims.offline_until {
        return Err("invalid license claims");
    }
    Ok(())
}

pub fn sign_refresh(
    license_id: &str,
    nonce: &str,
    device_private_key: &[u8],
) -> Result<String, &'static str> {
    let key = Ed25519KeyPair::from_pkcs8(device_private_key).map_err(|_| "invalid device key")?;
    Ok(hex_encode(
        key.sign(&refresh_bytes(license_id, nonce)?).as_ref(),
    ))
}

pub fn verify_refresh(request: &RefreshRequest) -> Result<(), &'static str> {
    let device = hex_decode::<32>(&request.device_public_key)?;
    let signature = hex_decode::<64>(&request.signature)?;
    signature::UnparsedPublicKey::new(&signature::ED25519, device)
        .verify(
            &refresh_bytes(&request.license_id, &request.nonce)?,
            &signature,
        )
        .map_err(|_| "invalid device signature")
}

fn refresh_bytes(license_id: &str, nonce: &str) -> Result<Vec<u8>, &'static str> {
    let _ = hex_decode::<16>(license_id)?;
    let _ = hex_decode::<16>(nonce)?;
    let mut bytes = REFRESH_CONTEXT.to_vec();
    bytes.extend_from_slice(license_id.as_bytes());
    bytes.push(b'\n');
    bytes.extend_from_slice(nonce.as_bytes());
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims(device: &str) -> LicenseClaims {
        LicenseClaims {
            protocol_version: PROTOCOL_VERSION,
            product: PRODUCT.to_owned(),
            license_id: hex_encode(&[1; 16]),
            device_public_key: device.to_owned(),
            binding_version: 1,
            issued_at: 100,
            offline_until: 200,
        }
    }

    #[test]
    fn validity_boundaries_wrong_issuer_and_every_claim_are_checked() {
        let issuer = generate_device_key().unwrap();
        let other_issuer = generate_device_key().unwrap();
        let device = public_key(&generate_device_key().unwrap()).unwrap();
        let issuer_public = public_key(&issuer).unwrap();
        let license = sign_license(claims(&device), &issuer).unwrap();
        for (time, valid) in [(99, false), (100, true), (199, true), (200, false)] {
            assert_eq!(
                verify_license(&license, &issuer_public, &device, time).is_ok(),
                valid
            );
        }
        assert!(
            verify_license(&license, &public_key(&other_issuer).unwrap(), &device, 150).is_err()
        );
        for field in 0..7 {
            let mut changed = license.clone();
            match field {
                0 => changed.claims.protocol_version += 1,
                1 => changed.claims.product.push('x'),
                2 => changed.claims.license_id = hex_encode(&[2; 16]),
                3 => changed.claims.device_public_key = hex_encode(&[3; 32]),
                4 => changed.claims.binding_version += 1,
                5 => changed.claims.issued_at += 1,
                _ => changed.claims.offline_until += 1,
            }
            assert!(verify_license(&changed, &issuer_public, &device, 150).is_err());
        }
    }

    #[test]
    fn malformed_claims_and_ambiguous_json_fail_closed() {
        let issuer = generate_device_key().unwrap();
        let device = public_key(&generate_device_key().unwrap()).unwrap();
        for field in 0..5 {
            let mut invalid = claims(&device);
            match field {
                0 => invalid.binding_version = 0,
                1 => invalid.offline_until = invalid.issued_at,
                2 => invalid.license_id = "not-an-id".to_owned(),
                3 => invalid.device_public_key = "00".to_owned(),
                _ => invalid.product = "another-product".to_owned(),
            }
            assert!(sign_license(invalid, &issuer).is_err());
        }
        let license = sign_license(claims(&device), &issuer).unwrap();
        let mut json = serde_json::to_value(&license).unwrap();
        json["claims"]["extra_entitlement"] = serde_json::json!(true);
        assert!(serde_json::from_value::<SignedLicense>(json).is_err());
        let duplicate = format!(
            "{{\"signature\":\"duplicate\",{}",
            &serde_json::to_string(&license).unwrap()[1..]
        );
        assert!(serde_json::from_str::<SignedLicense>(&duplicate).is_err());
    }

    #[test]
    fn refresh_proof_binds_license_and_device_and_rejects_malformed_fields() {
        let device = generate_device_key().unwrap();
        let id = hex_encode(&[1; 16]);
        let nonce = hex_encode(&[2; 16]);
        let request = RefreshRequest {
            license_id: id.clone(),
            device_public_key: public_key(&device).unwrap(),
            signature: sign_refresh(&id, &nonce, &device).unwrap(),
            nonce,
        };
        for field in 0..4 {
            let mut changed = request.clone();
            match field {
                0 => changed.license_id = hex_encode(&[3; 16]),
                1 => {
                    changed.device_public_key = public_key(&generate_device_key().unwrap()).unwrap()
                }
                2 => changed.nonce = "xx".to_owned(),
                _ => changed.signature = hex_encode(&[0; 64]),
            }
            assert!(verify_refresh(&changed).is_err());
        }
        assert!(hex_decode::<1>("é").is_err());
        assert!(hex_decode::<1>("AA").is_err());
    }

    #[test]
    fn signed_license_rejects_tampering_and_other_device() {
        let issuer = generate_device_key().unwrap();
        let device = generate_device_key().unwrap();
        let other = generate_device_key().unwrap();
        let claims = LicenseClaims {
            protocol_version: 1,
            product: PRODUCT.to_owned(),
            license_id: hex_encode(&[1; 16]),
            device_public_key: public_key(&device).unwrap(),
            binding_version: 1,
            issued_at: 100,
            offline_until: 200,
        };
        let license = sign_license(claims, &issuer).unwrap();
        let issuer_public = public_key(&issuer).unwrap();
        assert!(
            verify_license(&license, &issuer_public, &public_key(&device).unwrap(), 150).is_ok()
        );
        assert!(
            verify_license(&license, &issuer_public, &public_key(&other).unwrap(), 150).is_err()
        );
        let mut tampered = license.clone();
        tampered.claims.offline_until = 300;
        assert!(
            verify_license(
                &tampered,
                &issuer_public,
                &public_key(&device).unwrap(),
                150
            )
            .is_err()
        );
        assert!(
            verify_license(&license, &issuer_public, &public_key(&device).unwrap(), 201).is_err()
        );
    }

    #[test]
    fn refresh_signature_cannot_be_reused_for_another_nonce() {
        let device = generate_device_key().unwrap();
        let license_id = hex_encode(&[2; 16]);
        let nonce = hex_encode(&[3; 16]);
        let mut request = RefreshRequest {
            license_id: license_id.clone(),
            device_public_key: public_key(&device).unwrap(),
            signature: sign_refresh(&license_id, &nonce, &device).unwrap(),
            nonce,
        };
        assert!(verify_refresh(&request).is_ok());
        request.nonce = hex_encode(&[4; 16]);
        assert!(verify_refresh(&request).is_err());
    }
}
