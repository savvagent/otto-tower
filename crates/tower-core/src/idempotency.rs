//! Shared plumbing for the optional caller-supplied idempotency key on
//! `add_job` and `send_message`.
//!
//! The key never proves uniqueness by itself — a partial unique index on
//! `(org_id, idempotency_key)` does that, checked with a SAVEPOINT and a
//! retry on conflict exactly like 0015's ticket_ref index. This module only
//! owns validation and the payload fingerprint that tells "the same call,
//! replayed" apart from "a different call that happens to reuse a key" —
//! the latter must error loudly, never silently return the wrong row.

use crate::error::{Error, Result};
use sha2::{Digest, Sha256};

/// Long enough for a UUID or a short caller-chosen string; short enough that
/// the column cannot be used as unbounded free storage.
pub const MAX_KEY_LEN: usize = 200;

pub fn validate(key: &str) -> Result<()> {
    if key.trim().is_empty() {
        return Err(Error::Invalid("idempotency_key must not be empty".into()));
    }
    if key.len() > MAX_KEY_LEN {
        return Err(Error::Invalid(format!(
            "idempotency_key is {} bytes; the limit is {MAX_KEY_LEN}",
            key.len()
        )));
    }
    Ok(())
}

/// Fingerprint the fields that define "the same call". Not a secret —
/// SHA-256 here is for a stable, fixed-size comparison key, not
/// confidentiality. `serde_json::Value::Object` in this workspace serializes
/// with sorted keys (the `preserve_order` feature is not enabled), so this
/// is deterministic regardless of caller-supplied field order.
pub fn fingerprint(payload: &serde_json::Value) -> Vec<u8> {
    Sha256::digest(payload.to_string().as_bytes()).to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_empty_and_whitespace_only() {
        assert!(validate("").is_err());
        assert!(validate("   ").is_err());
    }

    #[test]
    fn validate_rejects_over_length() {
        let too_long = "x".repeat(MAX_KEY_LEN + 1);
        assert!(validate(&too_long).is_err());
    }

    #[test]
    fn validate_accepts_a_normal_key() {
        assert!(validate("retry-7f3a").is_ok());
        assert!(validate(&"x".repeat(MAX_KEY_LEN)).is_ok());
    }

    #[test]
    fn fingerprint_is_insensitive_to_object_key_order() {
        let a = serde_json::json!({ "title": "t", "repoId": "r" });
        let b = serde_json::json!({ "repoId": "r", "title": "t" });
        assert_eq!(fingerprint(&a), fingerprint(&b));
    }

    #[test]
    fn fingerprint_differs_for_different_payloads() {
        let a = serde_json::json!({ "title": "t1" });
        let b = serde_json::json!({ "title": "t2" });
        assert_ne!(fingerprint(&a), fingerprint(&b));
    }
}
