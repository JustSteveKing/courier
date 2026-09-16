//! Signing JSON Web Tokens for APIs that want one per request: HMAC with a shared secret,
//! or RSA and ECDSA with a private key.

use rust_i18n::t;
use serde::{Deserialize, Serialize};

use crate::encoding::base64_url_encode;
use crate::model::Variables;

/// The algorithms Courier can sign with, as they're named in a JWT header.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Algorithm {
    #[default]
    HS256,
    HS384,
    HS512,
    RS256,
    RS384,
    RS512,
    ES256,
}

impl Algorithm {
    pub const ALL: [Self; 7] = [
        Self::HS256,
        Self::HS384,
        Self::HS512,
        Self::RS256,
        Self::RS384,
        Self::RS512,
        Self::ES256,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::HS256 => "HS256",
            Self::HS384 => "HS384",
            Self::HS512 => "HS512",
            Self::RS256 => "RS256",
            Self::RS384 => "RS384",
            Self::RS512 => "RS512",
            Self::ES256 => "ES256",
        }
    }

    /// Whether the key is a shared secret rather than a PEM private key.
    pub fn is_shared_secret(self) -> bool {
        matches!(self, Self::HS256 | Self::HS384 | Self::HS512)
    }
}

/// Signs `claims` (a JSON object) and returns the token.
///
/// `key` is the shared secret for HS\*, or a PEM private key for RS\* and ES256. Claims that
/// are `{{variables}}` are already resolved by the caller.
pub fn sign(algorithm: Algorithm, key: &str, claims: &str, extra_header: &str) -> Result<String, String> {
    let claims = claims.trim();
    let claims: serde_json::Value = if claims.is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(claims).map_err(|e| t!("jwt.bad_claims", error = e.to_string()).to_string())?
    };
    if !claims.is_object() {
        return Err(t!("jwt.claims_not_object").to_string());
    }
    let mut header = serde_json::json!({ "alg": algorithm.name(), "typ": "JWT" });
    let extra_header = extra_header.trim();
    if !extra_header.is_empty() {
        let extra: serde_json::Value =
            serde_json::from_str(extra_header).map_err(|e| t!("jwt.bad_header", error = e.to_string()).to_string())?;
        match (header.as_object_mut(), extra.as_object()) {
            (Some(header), Some(extra)) => header.extend(extra.clone()),
            _ => return Err(t!("jwt.header_not_object").to_string()),
        }
    }

    let header = base64_url_encode(serde_json::to_string(&header).unwrap_or_default().as_bytes());
    let payload = base64_url_encode(serde_json::to_string(&claims).unwrap_or_default().as_bytes());
    let signing_input = format!("{header}.{payload}");
    let signature = self::signature(algorithm, key, signing_input.as_bytes())?;
    Ok(format!("{signing_input}.{}", base64_url_encode(&signature)))
}

fn signature(algorithm: Algorithm, key: &str, message: &[u8]) -> Result<Vec<u8>, String> {
    use hmac::Mac as _;
    if key.trim().is_empty() {
        return Err(t!("jwt.no_key").to_string());
    }
    match algorithm {
        Algorithm::HS256 => {
            let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(key.as_bytes()).map_err(|e| e.to_string())?;
            mac.update(message);
            Ok(mac.finalize().into_bytes().to_vec())
        }
        Algorithm::HS384 => {
            let mut mac = hmac::Hmac::<sha2::Sha384>::new_from_slice(key.as_bytes()).map_err(|e| e.to_string())?;
            mac.update(message);
            Ok(mac.finalize().into_bytes().to_vec())
        }
        Algorithm::HS512 => {
            let mut mac = hmac::Hmac::<sha2::Sha512>::new_from_slice(key.as_bytes()).map_err(|e| e.to_string())?;
            mac.update(message);
            Ok(mac.finalize().into_bytes().to_vec())
        }
        Algorithm::RS256 | Algorithm::RS384 | Algorithm::RS512 => rsa_signature(algorithm, key, message),
        Algorithm::ES256 => ecdsa_signature(key, message),
    }
}

fn rsa_private_key(key: &str) -> Result<rsa::RsaPrivateKey, String> {
    use rsa::pkcs1::DecodeRsaPrivateKey as _;
    use rsa::pkcs8::DecodePrivateKey as _;
    // Both "BEGIN PRIVATE KEY" (PKCS#8) and "BEGIN RSA PRIVATE KEY" (PKCS#1) are common.
    rsa::RsaPrivateKey::from_pkcs8_pem(key)
        .or_else(|_| rsa::RsaPrivateKey::from_pkcs1_pem(key))
        .map_err(|e| t!("jwt.bad_key", error = e.to_string()).to_string())
}

fn rsa_signature(algorithm: Algorithm, key: &str, message: &[u8]) -> Result<Vec<u8>, String> {
    use rsa::signature::{SignatureEncoding as _, Signer as _};
    let key = rsa_private_key(key)?;
    let signature = match algorithm {
        Algorithm::RS384 => rsa::pkcs1v15::SigningKey::<sha2::Sha384>::new(key)
            .sign(message)
            .to_vec(),
        Algorithm::RS512 => rsa::pkcs1v15::SigningKey::<sha2::Sha512>::new(key)
            .sign(message)
            .to_vec(),
        _ => rsa::pkcs1v15::SigningKey::<sha2::Sha256>::new(key)
            .sign(message)
            .to_vec(),
    };
    Ok(signature)
}

fn ecdsa_signature(key: &str, message: &[u8]) -> Result<Vec<u8>, String> {
    use p256::ecdsa::signature::Signer as _;
    use p256::pkcs8::DecodePrivateKey as _;
    let key = p256::ecdsa::SigningKey::from_pkcs8_pem(key)
        .map_err(|e| t!("jwt.bad_key", error = e.to_string()).to_string())?;
    let signature: p256::ecdsa::Signature = key.sign(message);
    // JWT wants the raw r‖s pair, not the DER encoding.
    Ok(signature.to_bytes().to_vec())
}

/// Replaces a request's `Auth::Jwt` with the header it produces, so resolving the request
/// itself stays free of signing.
pub fn authorize(file: &mut crate::model::RequestFile, variables: &Variables) -> Result<(), String> {
    let crate::model::Auth::Jwt {
        algorithm,
        key,
        claims,
        header,
        prefix,
    } = &file.auth
    else {
        return Ok(());
    };
    let sub = |text: &str| crate::model::interpolate(text, variables).0;
    let token = sign(*algorithm, &sub(key), &sub(claims), &sub(header))?;
    let prefix = sub(prefix);
    let value = match prefix.trim() {
        "" => token,
        prefix => format!("{prefix} {token}"),
    };
    file.auth = crate::model::Auth::ApiKey {
        name: "Authorization".into(),
        value,
        in_query: false,
    };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The signature here was checked against Python's hmac, so this pins the whole
    /// encoding: header, payload and base64url without padding.
    #[test]
    fn signs_a_known_token() {
        let token = sign(Algorithm::HS256, "secret", r#"{"sub":"1234567890"}"#, "").unwrap();
        assert_eq!(
            token,
            concat!(
                "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.",
                "eyJzdWIiOiIxMjM0NTY3ODkwIn0.",
                "Rq8IxqeX7eA6GgYxlcHdPFVRNFFZc5rEI3MQTZZbK3I"
            )
        );
        assert!(!token.contains('='), "base64url, no padding");
        // The longer HMACs differ only in the algorithm and the signature's length.
        let long = sign(Algorithm::HS512, "secret", r#"{"sub":"1234567890"}"#, "").unwrap();
        assert!(decode(long.split('.').next().unwrap()).contains("HS512"));
        assert_eq!(split(&long).1.len(), 64);
    }

    #[test]
    fn puts_extra_header_fields_in_and_checks_the_claims() {
        let token = sign(Algorithm::HS256, "secret", r#"{"iss":"courier"}"#, r#"{"kid":"key-1"}"#).unwrap();
        let header = token.split('.').next().unwrap();
        let decoded = decode(header);
        assert!(decoded.contains("\"kid\":\"key-1\""), "{decoded}");
        assert!(decoded.contains("\"alg\":\"HS256\""), "{decoded}");

        assert!(sign(Algorithm::HS256, "secret", "not json", "").is_err());
        assert!(
            sign(Algorithm::HS256, "secret", "[1,2]", "").is_err(),
            "claims are an object"
        );
        assert!(sign(Algorithm::HS256, "", "{}", "").is_err(), "a key is needed");
        assert!(
            sign(Algorithm::HS256, "secret", "", "").is_ok(),
            "no claims is an empty object"
        );
    }

    #[test]
    fn signs_with_rsa_and_ecdsa_keys() {
        use rsa::pkcs8::EncodePrivateKey as _;
        let mut rng = rand::thread_rng();
        let rsa_key = rsa::RsaPrivateKey::new(&mut rng, 2048).unwrap();
        let rsa_pem = rsa_key.to_pkcs8_pem(rsa::pkcs8::LineEnding::LF).unwrap().to_string();
        let token = sign(Algorithm::RS256, &rsa_pem, r#"{"sub":"a"}"#, "").unwrap();
        let (message, signature) = split(&token);
        use rsa::signature::Verifier as _;
        let verifying = rsa::pkcs1v15::VerifyingKey::<sha2::Sha256>::new(rsa_key.to_public_key());
        assert!(
            verifying
                .verify(
                    message.as_bytes(),
                    &rsa::pkcs1v15::Signature::try_from(signature.as_slice()).unwrap()
                )
                .is_ok(),
            "the RS256 signature verifies"
        );

        let ec_key = p256::ecdsa::SigningKey::random(&mut rand::thread_rng());
        let ec_pem = p256::pkcs8::EncodePrivateKey::to_pkcs8_pem(&ec_key, p256::pkcs8::LineEnding::LF)
            .unwrap()
            .to_string();
        let token = sign(Algorithm::ES256, &ec_pem, r#"{"sub":"a"}"#, "").unwrap();
        let (message, signature) = split(&token);
        assert_eq!(signature.len(), 64, "ES256 signatures are the raw r and s");
        let signature = p256::ecdsa::Signature::from_slice(&signature).unwrap();
        assert!(
            ec_key.verifying_key().verify(message.as_bytes(), &signature).is_ok(),
            "the ES256 signature verifies"
        );

        assert!(sign(Algorithm::RS256, "not a pem", "{}", "").is_err());
    }

    fn decode(part: &str) -> String {
        let mut padded = part.replace('-', "+").replace('_', "/");
        while !padded.len().is_multiple_of(4) {
            padded.push('=');
        }
        String::from_utf8(crate::encoding::base64_decode(&padded).unwrap()).unwrap()
    }

    /// The signed part of a token, and its signature bytes.
    fn split(token: &str) -> (String, Vec<u8>) {
        let at = token.rfind('.').unwrap();
        let mut padded = token[at + 1..].replace('-', "+").replace('_', "/");
        while !padded.len().is_multiple_of(4) {
            padded.push('=');
        }
        (
            token[..at].to_string(),
            crate::encoding::base64_decode(&padded).unwrap(),
        )
    }
}
