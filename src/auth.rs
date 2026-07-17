//! Kalshi API request signing.
//!
//! Kalshi authenticates with an **API key ID (UUID)** plus an **RSA private
//! key**. Each private request carries three headers:
//!
//! - `KALSHI-ACCESS-KEY`       — the API key ID.
//! - `KALSHI-ACCESS-TIMESTAMP` — current time in **milliseconds** since epoch.
//! - `KALSHI-ACCESS-SIGNATURE` — `base64( RSA-PSS/SHA-256( timestamp + METHOD + path ) )`.
//!
//! The signed message is the concatenation of the millisecond timestamp string,
//! the uppercase HTTP method, and the request path (which includes the
//! `/trade-api/v2` prefix and excludes any query string). PSS uses SHA-256 with
//! salt length equal to the digest length.

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::pss::SigningKey;
use rsa::signature::{RandomizedSigner, SignatureEncoding};
use rsa::RsaPrivateKey;
use sha2::Sha256;

/// Signs Kalshi API requests. Cheap to clone (holds an `RsaPrivateKey`).
#[derive(Clone)]
pub struct Signer {
    key_id: String,
    key: RsaPrivateKey,
}

/// The three headers required for an authenticated Kalshi request.
pub struct SignedHeaders {
    pub key_id: String,
    pub timestamp_ms: String,
    pub signature_b64: String,
}

impl Signer {
    /// Build a signer from the API key ID and an RSA private key in PEM form.
    /// Accepts both PKCS#1 (`BEGIN RSA PRIVATE KEY`) and PKCS#8
    /// (`BEGIN PRIVATE KEY`) encodings.
    pub fn new(key_id: impl Into<String>, private_key_pem: &str) -> Result<Self> {
        let key = RsaPrivateKey::from_pkcs1_pem(private_key_pem)
            .or_else(|_| RsaPrivateKey::from_pkcs8_pem(private_key_pem))
            .context("parsing RSA private key (tried PKCS#1 and PKCS#8 PEM)")?;
        Ok(Self { key_id: key_id.into(), key })
    }

    /// The exact message that gets signed: `timestamp + METHOD + path`.
    /// `method` is uppercased; `path` must include `/trade-api/v2` and exclude
    /// the query string.
    fn message(timestamp_ms: &str, method: &str, path: &str) -> String {
        format!("{}{}{}", timestamp_ms, method.to_uppercase(), path)
    }

    /// Sign a request, producing the three headers. `timestamp_ms` is supplied
    /// by the caller so this is deterministic and testable; `client` fills it
    /// with the current wall-clock time.
    pub fn sign(&self, method: &str, path: &str, timestamp_ms: u64) -> Result<SignedHeaders> {
        let ts = timestamp_ms.to_string();
        let msg = Self::message(&ts, method, path);

        let signing_key = SigningKey::<Sha256>::new(self.key.clone());
        let mut rng = rand::thread_rng();
        let signature = signing_key.sign_with_rng(&mut rng, msg.as_bytes());

        Ok(SignedHeaders {
            key_id: self.key_id.clone(),
            timestamp_ms: ts,
            signature_b64: STANDARD.encode(signature.to_bytes()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::pss::VerifyingKey;
    use rsa::signature::Verifier;
    use rsa::pkcs1::EncodeRsaPrivateKey;
    use rsa::RsaPublicKey;

    /// Generate a small test key and return its PEM so the signer can parse it
    /// exactly as it would a real Kalshi key.
    fn test_key() -> (RsaPrivateKey, String) {
        let mut rng = rand::thread_rng();
        // 2048 is Kalshi's size; keep it real so the test exercises the true path.
        let key = RsaPrivateKey::new(&mut rng, 2048).expect("keygen");
        let pem = key.to_pkcs1_pem(rsa::pkcs1::LineEnding::LF).unwrap().to_string();
        (key, pem)
    }

    #[test]
    fn message_is_timestamp_method_path_with_upper_method() {
        assert_eq!(
            Signer::message("1700000000000", "get", "/trade-api/v2/portfolio/balance"),
            "1700000000000GET/trade-api/v2/portfolio/balance"
        );
    }

    #[test]
    fn signature_verifies_against_public_key() {
        let (key, pem) = test_key();
        let signer = Signer::new("key-id-123", &pem).unwrap();

        let headers = signer
            .sign("GET", "/trade-api/v2/portfolio/balance", 1700000000000)
            .unwrap();

        assert_eq!(headers.key_id, "key-id-123");
        assert_eq!(headers.timestamp_ms, "1700000000000");

        // Recompute the message and verify the signature with the public half.
        let msg = Signer::message("1700000000000", "GET", "/trade-api/v2/portfolio/balance");
        let sig_bytes = STANDARD.decode(&headers.signature_b64).unwrap();
        let signature = rsa::pss::Signature::try_from(sig_bytes.as_slice()).unwrap();

        let verifying: VerifyingKey<Sha256> = VerifyingKey::new(RsaPublicKey::from(&key));
        verifying
            .verify(msg.as_bytes(), &signature)
            .expect("signature must verify against the public key");
    }

    #[test]
    fn wrong_message_fails_verification() {
        let (key, pem) = test_key();
        let signer = Signer::new("k", &pem).unwrap();
        let headers = signer.sign("GET", "/trade-api/v2/markets", 111).unwrap();

        let wrong = Signer::message("111", "POST", "/trade-api/v2/markets");
        let sig_bytes = STANDARD.decode(&headers.signature_b64).unwrap();
        let signature = rsa::pss::Signature::try_from(sig_bytes.as_slice()).unwrap();
        let verifying: VerifyingKey<Sha256> = VerifyingKey::new(RsaPublicKey::from(&key));
        assert!(verifying.verify(wrong.as_bytes(), &signature).is_err());
    }
}
