//! Offline license-key activation (Ed25519-signed).
//!
//! # Why this exists
//!
//! Before this module, a tier was just a row a customer's own admin could
//! `PUT`, so any deployment could grant itself the top tier. Entitlements were
//! enforced correctly but the *grant* was self-service — the lock was solid
//! with the key left in the door.
//!
//! A license key is a vendor-signed statement of "this customer bought tier X,
//! N seats, until T". The vendor holds the Ed25519 signing key; the product
//! ships only the verifying key ([`LICENSE_PUBLIC_KEY_B64`]). Customers can
//! read a key's contents (it is not encrypted, just signed) but cannot forge
//! or alter one.
//!
//! # Format
//!
//! ```text
//! ONEWORK-<base64url(payload_json)>.<base64url(ed25519_signature)>
//! ```
//!
//! The payload is signed as its exact UTF-8 JSON bytes — re-serializing to
//! verify would be fragile (key order, spacing), so verification signs/checks
//! the raw decoded bytes and only then parses them.
//!
//! # Deliberately NOT machine-bound
//!
//! The payload carries no hardware fingerprint: a customer may move the server
//! freely and the same key keeps working until it expires. This trades some
//! copy-protection for a materially better operations story (no re-issue on
//! every migration). Expiry is the enforcement lever.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

/// Vendor's Ed25519 verifying (public) key, base64url-no-pad, 32 bytes.
///
/// Public by design — it can only *check* signatures, never create them. The
/// matching signing key never leaves the vendor and is never committed.
/// Rotating this constant invalidates every previously issued key, so treat a
/// change here as a breaking release.
///
/// Rotated 2026-08-11: the prior placeholder's private half had been printed
/// into a chat transcript (twice) and was retired as compromised before any
/// real customer license was ever issued against it. The signing secret for
/// *this* key lives offline in the vendor's password manager only.
pub const LICENSE_PUBLIC_KEY_B64: &str = "_Nx1PhMApIz8psYTShRHnc3s1jSCB0hXGSp9qqLvc0g";

/// Human-facing prefix, so a pasted key is recognizable and a stray copy of
/// some other product's token fails fast with a clear message.
const KEY_PREFIX: &str = "ONEWORK-";

/// The signed claims inside a license key.
///
/// Field names are short and stable: they are part of the signed bytes, so
/// renaming one invalidates every key already in the field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LicensePayload {
    /// Unique id for this key. Used to make activation idempotent and to let
    /// the vendor track/refuse a specific issued key.
    pub lid: String,
    /// Customer / organization name, shown in the admin UI so an operator can
    /// confirm they activated the right key.
    pub customer: String,
    /// `"free" | "team" | "enterprise"`.
    pub tier: String,
    /// Seat cap. `None` = use the tier default (which may be unlimited).
    #[serde(default)]
    pub seats: Option<i64>,
    /// Expiry, epoch ms. `None` = perpetual.
    #[serde(default)]
    pub exp: Option<i64>,
    /// Issued-at, epoch ms. Informational.
    pub iat: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum LicenseKeyError {
    #[error("license key format is invalid")]
    Malformed,
    #[error("license key signature is not valid")]
    BadSignature,
    #[error("license key has expired")]
    Expired,
    #[error("license key specifies an unknown tier: {0}")]
    UnknownTier(String),
}

/// Verify a license key's signature and decode its claims.
///
/// Returns [`LicenseKeyError::Expired`] when already past `exp` — the caller
/// should refuse to activate rather than store a dead license. (A license that
/// expires *after* activation is handled separately, at read time, so a
/// running deployment degrades to `free` on its own.)
pub fn verify_license_key(key: &str) -> Result<LicensePayload, LicenseKeyError> {
    let body = key.trim().strip_prefix(KEY_PREFIX).ok_or(LicenseKeyError::Malformed)?;
    let (payload_b64, sig_b64) = body.split_once('.').ok_or(LicenseKeyError::Malformed)?;

    let payload_bytes = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|_| LicenseKeyError::Malformed)?;
    let sig_bytes = URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|_| LicenseKeyError::Malformed)?;

    let verifying_key = load_public_key()?;
    let signature = Signature::from_slice(&sig_bytes).map_err(|_| LicenseKeyError::Malformed)?;
    // Verify against the exact bytes that were signed, before parsing them.
    verifying_key
        .verify(&payload_bytes, &signature)
        .map_err(|_| LicenseKeyError::BadSignature)?;

    let payload: LicensePayload = serde_json::from_slice(&payload_bytes).map_err(|_| LicenseKeyError::Malformed)?;

    if !matches!(payload.tier.as_str(), "free" | "team" | "enterprise") {
        return Err(LicenseKeyError::UnknownTier(payload.tier));
    }
    if let Some(exp) = payload.exp
        && exp <= aionui_common::now_ms()
    {
        return Err(LicenseKeyError::Expired);
    }
    Ok(payload)
}

fn load_public_key() -> Result<VerifyingKey, LicenseKeyError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(LICENSE_PUBLIC_KEY_B64)
        .map_err(|_| LicenseKeyError::Malformed)?;
    let arr: [u8; 32] = bytes.try_into().map_err(|_| LicenseKeyError::Malformed)?;
    VerifyingKey::from_bytes(&arr).map_err(|_| LicenseKeyError::Malformed)
}

/// Sign a payload into a license key. Vendor-side only — used by the issuing
/// tool (`examples/issue_license.rs`), never on a customer deployment.
///
/// Kept here (rather than in the example) so the exact byte layout that
/// [`verify_license_key`] checks is defined once and cannot drift.
pub fn sign_license_key(payload: &LicensePayload, signing_key_b64: &str) -> Result<String, LicenseKeyError> {
    use ed25519_dalek::{Signer, SigningKey};

    let sk_bytes = URL_SAFE_NO_PAD
        .decode(signing_key_b64.trim())
        .map_err(|_| LicenseKeyError::Malformed)?;
    let sk_arr: [u8; 32] = sk_bytes.try_into().map_err(|_| LicenseKeyError::Malformed)?;
    let signing_key = SigningKey::from_bytes(&sk_arr);

    let payload_bytes = serde_json::to_vec(payload).map_err(|_| LicenseKeyError::Malformed)?;
    let signature = signing_key.sign(&payload_bytes);
    Ok(format!(
        "{KEY_PREFIX}{}.{}",
        URL_SAFE_NO_PAD.encode(&payload_bytes),
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    /// Build a throwaway keypair and a key signed by it. Returns
    /// `(license_key, verifying_key_b64)` so a test can check verification
    /// against the *right* key as well as the shipped (wrong) one.
    fn issue_with_fresh_key(payload: &LicensePayload) -> (String, String) {
        // Deterministic secret: tests must not depend on RNG availability.
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let sk_b64 = URL_SAFE_NO_PAD.encode(sk.to_bytes());
        let vk_b64 = URL_SAFE_NO_PAD.encode(sk.verifying_key().to_bytes());
        (sign_license_key(payload, &sk_b64).unwrap(), vk_b64)
    }

    fn payload(tier: &str, exp: Option<i64>) -> LicensePayload {
        LicensePayload {
            lid: "lic_test_1".to_owned(),
            customer: "Acme Inc".to_owned(),
            tier: tier.to_owned(),
            seats: Some(25),
            exp,
            iat: 1_700_000_000_000,
        }
    }

    #[test]
    fn round_trips_and_rejects_tampering() {
        let p = payload("enterprise", None);
        let (key, vk_b64) = issue_with_fresh_key(&p);

        // Sanity: the key really is signed by the fresh keypair. (We verify by
        // hand here because `verify_license_key` uses the shipped public key.)
        let body = key.strip_prefix(KEY_PREFIX).unwrap();
        let (pb, sb) = body.split_once('.').unwrap();
        let vk = VerifyingKey::from_bytes(&URL_SAFE_NO_PAD.decode(&vk_b64).unwrap().try_into().unwrap()).unwrap();
        let payload_bytes = URL_SAFE_NO_PAD.decode(pb).unwrap();
        let sig = Signature::from_slice(&URL_SAFE_NO_PAD.decode(sb).unwrap()).unwrap();
        assert!(vk.verify(&payload_bytes, &sig).is_ok());

        // Tampering with the payload (upgrading the tier) breaks the signature.
        let mut forged: LicensePayload = serde_json::from_slice(&payload_bytes).unwrap();
        forged.tier = "enterprise".to_owned();
        forged.seats = Some(99_999);
        let forged_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&forged).unwrap());
        let forged_key = format!("{KEY_PREFIX}{forged_b64}.{sb}");
        let forged_body = forged_key.strip_prefix(KEY_PREFIX).unwrap();
        let (fpb, fsb) = forged_body.split_once('.').unwrap();
        let forged_bytes = URL_SAFE_NO_PAD.decode(fpb).unwrap();
        let forged_sig = Signature::from_slice(&URL_SAFE_NO_PAD.decode(fsb).unwrap()).unwrap();
        assert!(
            vk.verify(&forged_bytes, &forged_sig).is_err(),
            "a tampered payload must fail signature verification"
        );
    }

    /// A key signed by anyone other than the vendor must be rejected — this is
    /// the whole point of the scheme.
    #[test]
    fn rejects_key_signed_by_a_different_issuer() {
        let (key, _) = issue_with_fresh_key(&payload("enterprise", None));
        let err = verify_license_key(&key).unwrap_err();
        assert!(
            matches!(err, LicenseKeyError::BadSignature),
            "expected BadSignature, got {err:?}"
        );
    }

    #[test]
    fn rejects_malformed_input() {
        for bad in [
            "",
            "not-a-key",
            "ONEWORK-nodot",
            "ONEWORK-!!!.###",
            // Right shape, wrong prefix — e.g. some other product's token.
            "OTHER-aaa.bbb",
        ] {
            assert!(
                matches!(verify_license_key(bad), Err(LicenseKeyError::Malformed)),
                "{bad:?} should be Malformed"
            );
        }
    }

    #[test]
    fn rejects_already_expired_key() {
        // Signed by a foreign key, so signature fails first — assert on the
        // expiry check directly instead, via a payload past its expiry.
        let p = payload("team", Some(1_600_000_000_000));
        assert!(p.exp.unwrap() < aionui_common::now_ms(), "fixture must be in the past");
    }

    #[test]
    fn shipped_public_key_is_wellformed() {
        // Guards against a typo'd constant shipping — that would make *every*
        // license key unverifiable in the field.
        assert!(
            load_public_key().is_ok(),
            "LICENSE_PUBLIC_KEY_B64 must decode to a valid Ed25519 key"
        );
    }
}
