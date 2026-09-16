//! HTTP Digest authentication (RFC 7616, and the RFC 2617 shape still in the wild).
//!
//! Unlike the other kinds, Digest needs the server's challenge first: the request goes out
//! once without credentials, comes back 401 with a nonce, and goes out again signed.

use md5::Digest as _;
use rust_i18n::t;

use crate::http::Request;
use crate::model::{Auth, Variables, interpolate};

/// What the server asked for in `WWW-Authenticate`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Challenge {
    pub realm: String,
    pub nonce: String,
    pub opaque: String,
    /// `auth`, `auth-int`, or empty for the old RFC 2069 shape.
    pub qop: String,
    /// `MD5`, `MD5-sess`, `SHA-256`, `SHA-256-sess`.
    pub algorithm: String,
}

impl Challenge {
    /// Reads the `Digest …` part of a `WWW-Authenticate` header. A server may offer several
    /// schemes; only Digest is picked up here.
    pub fn parse(header: &str) -> Option<Self> {
        // A server may offer several schemes in one header; take everything after "Digest ".
        let at = header.find("Digest ")?;
        let rest = &header[at + "Digest ".len()..];
        let mut challenge = Self::default();
        for pair in split_fields(rest) {
            let Some((name, value)) = pair.split_once('=') else {
                continue;
            };
            let value = value.trim().trim_matches('"').to_string();
            match name.trim().to_ascii_lowercase().as_str() {
                "realm" => challenge.realm = value,
                "nonce" => challenge.nonce = value,
                "opaque" => challenge.opaque = value,
                // A server may offer "auth,auth-int"; plain auth is the one to use.
                "qop" if value.split(',').any(|q| q.trim() == "auth") => challenge.qop = "auth".into(),
                "qop" => challenge.qop = value,
                "algorithm" => challenge.algorithm = value,
                _ => {}
            }
        }
        (!challenge.nonce.is_empty()).then_some(challenge)
    }
}

/// Splits `a="x,y", b=z` on commas that aren't inside quotes.
fn split_fields(text: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for c in text.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                current.push(c);
            }
            ',' if !quoted => fields.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    fields.push(current);
    fields
}

fn hash(algorithm: &str, text: &str) -> String {
    if algorithm.to_ascii_uppercase().starts_with("SHA-256") {
        let digest = sha2::Sha256::digest(text.as_bytes());
        digest.iter().map(|byte| format!("{byte:02x}")).collect()
    } else {
        let digest = md5::Md5::digest(text.as_bytes());
        digest.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

/// Builds the `Authorization` header for a challenge. `cnonce` and `nc` are given rather
/// than generated so this can be checked against the published examples.
pub fn header_for(
    challenge: &Challenge,
    username: &str,
    password: &str,
    method: &str,
    uri: &str,
    cnonce: &str,
    nc: u32,
) -> String {
    let algorithm = if challenge.algorithm.is_empty() {
        "MD5".to_string()
    } else {
        challenge.algorithm.clone()
    };
    let mut ha1 = hash(&algorithm, &format!("{username}:{}:{password}", challenge.realm));
    if algorithm.to_ascii_lowercase().ends_with("-sess") {
        ha1 = hash(&algorithm, &format!("{ha1}:{}:{cnonce}", challenge.nonce));
    }
    let ha2 = hash(&algorithm, &format!("{}:{uri}", method.to_ascii_uppercase()));
    let nc = format!("{nc:08x}");

    let response = if challenge.qop.is_empty() {
        // RFC 2069: no qop, no counter.
        hash(&algorithm, &format!("{ha1}:{}:{ha2}", challenge.nonce))
    } else {
        hash(
            &algorithm,
            &format!("{ha1}:{}:{nc}:{cnonce}:{}:{ha2}", challenge.nonce, challenge.qop),
        )
    };

    let mut header = format!(
        "Digest username=\"{username}\", realm=\"{}\", nonce=\"{}\", uri=\"{uri}\", response=\"{response}\"",
        challenge.realm, challenge.nonce
    );
    if !challenge.algorithm.is_empty() {
        header.push_str(&format!(", algorithm={algorithm}"));
    }
    if !challenge.qop.is_empty() {
        header.push_str(&format!(", qop={}, nc={nc}, cnonce=\"{cnonce}\"", challenge.qop));
    }
    if !challenge.opaque.is_empty() {
        header.push_str(&format!(", opaque=\"{}\"", challenge.opaque));
    }
    header
}

/// Asks the server for a challenge and signs `request` with it. Called after the request is
/// resolved, since the signature covers the method and path that actually go out.
pub async fn apply(auth: &Auth, request: &mut Request, variables: &Variables) -> Result<(), String> {
    let Auth::Digest { username, password } = auth else {
        return Ok(());
    };
    let sub = |text: &str| interpolate(text, variables).0;
    let (username, password) = (sub(username), sub(password));
    let challenge = self::challenge(request).await?;
    let uri = uri_of(&request.url)?;
    let header = header_for(&challenge, &username, &password, &request.method, &uri, &cnonce(), 1);
    request
        .headers
        .retain(|(name, _)| !name.eq_ignore_ascii_case("authorization"));
    request.headers.push(("Authorization".into(), header));
    Ok(())
}

/// The path and query the digest is computed over.
fn uri_of(url: &str) -> Result<String, String> {
    let parsed = url::Url::parse(url).map_err(|e| t!("digest.bad_url", error = e.to_string()).to_string())?;
    Ok(match parsed.query() {
        Some(query) => format!("{}?{query}", parsed.path()),
        None => parsed.path().to_string(),
    })
}

fn cnonce() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Sends the request without credentials to collect the challenge. The server answers 401
/// without doing the work, which is how Digest is meant to go.
async fn challenge(request: &Request) -> Result<Challenge, String> {
    let client = crate::transport::plain_client();
    let method = reqwest::Method::from_bytes(request.method.as_bytes())
        .map_err(|e| t!("digest.bad_request", error = e.to_string()).to_string())?;
    let mut builder = client.request(method, &request.url);
    for (name, value) in &request.headers {
        if !name.eq_ignore_ascii_case("authorization") {
            builder = builder.header(name, value);
        }
    }
    let header = crate::transport::on_runtime(async move {
        let response = builder
            .send()
            .await
            .map_err(|e| t!("digest.no_challenge", error = e.to_string()).to_string())?;
        Ok::<_, String>(
            response
                .headers()
                .get(reqwest::header::WWW_AUTHENTICATE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string),
        )
    })
    .await?;
    let header = header.ok_or_else(|| t!("digest.not_offered").to_string())?;
    Challenge::parse(&header).ok_or_else(|| t!("digest.bad_challenge", header = header).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_rfc_example() {
        // RFC 2617, section 3.5: the example every Digest implementation is checked against.
        let challenge = Challenge::parse(
            "Digest realm=\"testrealm@host.com\", qop=\"auth,auth-int\", \
             nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\", opaque=\"5ccc069c403ebaf9f0171e9517f40e41\"",
        )
        .unwrap();
        assert_eq!(challenge.realm, "testrealm@host.com");
        assert_eq!(challenge.qop, "auth", "plain auth is picked out of the offer");
        assert!(challenge.algorithm.is_empty(), "the server didn't name one");

        let header = header_for(
            &challenge,
            "Mufasa",
            "Circle Of Life",
            "GET",
            "/dir/index.html",
            "0a4f113b",
            1,
        );
        assert!(
            header.contains("response=\"6629fae49393a05397450978507c4ef1\""),
            "the published response for this example:\n{header}"
        );
        assert!(header.contains("nc=00000001"), "{header}");
        assert!(
            header.contains("opaque=\"5ccc069c403ebaf9f0171e9517f40e41\""),
            "{header}"
        );
        assert!(!header.contains("algorithm="), "nothing invented for algorithm");
    }

    #[test]
    fn handles_sha_256_and_the_old_shape() {
        // RFC 7616 section 3.9.1, the SHA-256 example.
        let challenge = Challenge::parse(
            "Digest username=\"Mufasa\", realm=\"http-auth@example.org\", algorithm=SHA-256, \
             nonce=\"7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v\", \
             opaque=\"FQhe/qaU925kfnzjCev0ciny7QMkPqMAFRtzCUYo5tdS\", qop=\"auth\"",
        )
        .unwrap();
        let header = header_for(
            &challenge,
            "Mufasa",
            "Circle of Life",
            "GET",
            "/dir/index.html",
            "f2/wE4q74E6zIJEtWaHKaf5wv/H5QzzpXusqGemxURZJ",
            1,
        );
        assert!(
            header.contains("response=\"753927fa0e85d155564e2e272a28d1802ca10daf4496794697cf8db5856cb6c1\""),
            "the published SHA-256 response:\n{header}"
        );
        assert!(header.contains("algorithm=SHA-256"), "{header}");

        // An RFC 2069 server offers no qop, and then there's no counter either.
        let old = Challenge::parse("Digest realm=\"r\", nonce=\"n\"").unwrap();
        let header = header_for(&old, "u", "p", "GET", "/", "cn", 1);
        assert!(!header.contains("qop=") && !header.contains("nc="), "{header}");
        assert!(header.contains(&format!("response=\"{}\"", {
            let ha1 = hash("MD5", "u:r:p");
            let ha2 = hash("MD5", "GET:/");
            hash("MD5", &format!("{ha1}:n:{ha2}"))
        })));

        assert!(Challenge::parse("Basic realm=\"r\"").is_none(), "only Digest is read");
        assert!(Challenge::parse("Digest realm=\"r\"").is_none(), "a nonce is required");
    }

    #[test]
    fn takes_the_path_and_query_from_the_url() {
        assert_eq!(uri_of("https://api.test/dir/index.html").unwrap(), "/dir/index.html");
        assert_eq!(uri_of("https://api.test/search?q=1&b=2").unwrap(), "/search?q=1&b=2");
        assert_eq!(uri_of("https://api.test").unwrap(), "/");
        assert!(uri_of("not a url").is_err());
    }
}
