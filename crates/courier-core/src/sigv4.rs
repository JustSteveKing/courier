//! AWS Signature Version 4: signing a request for S3, or anything else that speaks the AWS
//! signing protocol (MinIO, Cloudflare R2, Backblaze B2's S3 API).

use hmac::Mac as _;
use rust_i18n::t;
use sha2::{Digest as _, Sha256};

use crate::http::Request;

/// Who is signing, and for what.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    /// For temporary credentials from STS.
    pub session_token: String,
    pub region: String,
    pub service: String,
}

/// Signs `request` in place: adds the date, the payload hash and the Authorization header.
/// `now` is the signing time, as `YYYYMMDDTHHMMSSZ`.
pub fn sign(request: &mut Request, credentials: &Credentials, now: &str) -> Result<(), String> {
    if credentials.access_key_id.trim().is_empty() || credentials.secret_access_key.trim().is_empty() {
        return Err(t!("sigv4.no_credentials").to_string());
    }
    let region = non_empty(&credentials.region, "us-east-1");
    let service = non_empty(&credentials.service, "s3");
    let date = now.get(..8).unwrap_or_default().to_string();

    let url = url::Url::parse(&request.url).map_err(|e| t!("sigv4.bad_url", error = e.to_string()).to_string())?;
    let host = match (url.host_str(), url.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_string(),
        (None, _) => return Err(t!("sigv4.bad_url", error = "no host").to_string()),
    };
    let payload_hash = hex(&Sha256::digest(request.body.as_bytes()));

    // Headers AWS needs to see, unless the request already carries them.
    set_header(&mut request.headers, "host", &host);
    set_header(&mut request.headers, "x-amz-date", now);
    // S3 wants the payload hash as a header; the other services don't ask for it.
    if service == "s3" {
        set_header(&mut request.headers, "x-amz-content-sha256", &payload_hash);
    }
    if !credentials.session_token.trim().is_empty() {
        set_header(&mut request.headers, "x-amz-security-token", &credentials.session_token);
    }

    // Every header is signed, lowercased and sorted, with their values trimmed. The
    // Authorization header is the one thing left out: it's what we're about to write, and
    // signing again would otherwise sign the last signature.
    let mut signed: Vec<(String, String)> = request
        .headers
        .iter()
        .filter(|(name, _)| !name.eq_ignore_ascii_case("authorization"))
        .map(|(name, value)| {
            (
                name.to_ascii_lowercase(),
                value.split_whitespace().collect::<Vec<_>>().join(" "),
            )
        })
        .collect();
    signed.sort_by(|a, b| a.0.cmp(&b.0));
    let signed_headers = signed
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical_headers = signed
        .iter()
        .map(|(name, value)| format!("{name}:{value}\n"))
        .collect::<String>();

    let canonical_request = format!(
        "{}\n{}\n{}\n{canonical_headers}\n{signed_headers}\n{payload_hash}",
        request.method.to_ascii_uppercase(),
        canonical_path(url.path()),
        canonical_query(&url),
    );
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{now}\n{scope}\n{}",
        hex(&Sha256::digest(canonical_request.as_bytes()))
    );

    let key = signing_key(&credentials.secret_access_key, &date, region, service);
    let signature = hex(&hmac(&key, to_sign.as_bytes()));
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        credentials.access_key_id
    );
    set_header(&mut request.headers, "Authorization", &authorization);
    Ok(())
}

/// Signs `request` when its auth is `Auth::AwsSigV4`. Unlike the other kinds this happens
/// after the request is resolved: the signature covers the final URL, headers and body.
pub fn apply(
    auth: &crate::model::Auth,
    request: &mut Request,
    variables: &crate::model::Variables,
) -> Result<(), String> {
    let crate::model::Auth::AwsSigV4 {
        access_key_id,
        secret_access_key,
        session_token,
        region,
        service,
    } = auth
    else {
        return Ok(());
    };
    let sub = |text: &str| crate::model::interpolate(text, variables).0.trim().to_string();
    let credentials = Credentials {
        access_key_id: sub(access_key_id),
        secret_access_key: sub(secret_access_key),
        session_token: sub(session_token),
        region: sub(region),
        service: sub(service),
    };
    sign(request, &credentials, &timestamp(std::time::SystemTime::now()))
}

/// The current time in AWS's format.
pub fn timestamp(at: std::time::SystemTime) -> String {
    let seconds = at
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let (year, month, day, hour, minute, second) = civil_from_unix(seconds);
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z")
}

/// Unix seconds to a UTC date and time, by the usual days-from-civil algorithm.
fn civil_from_unix(seconds: u64) -> (u64, u64, u64, u64, u64, u64) {
    let days = seconds / 86_400;
    let time = seconds % 86_400;
    let z = days + 719_468;
    let era = z / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day, time / 3600, time % 3600 / 60, time % 60)
}

fn non_empty<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    let value = value.trim();
    if value.is_empty() { fallback } else { value }
}

fn set_header(headers: &mut Vec<(String, String)>, name: &str, value: &str) {
    if let Some((_, existing)) = headers.iter_mut().find(|(header, _)| header.eq_ignore_ascii_case(name)) {
        *existing = value.to_string();
    } else {
        headers.push((name.to_string(), value.to_string()));
    }
}

/// The path, with each segment encoded the way AWS expects (and `/` for an empty path).
fn canonical_path(path: &str) -> String {
    if path.is_empty() {
        return "/".into();
    }
    path.split('/')
        .map(|segment| {
            // The path arrives already percent-encoded from the URL parser, so only the
            // characters AWS treats differently are touched.
            segment.replace('+', "%2B")
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Query parameters sorted by name, then value, each encoded strictly.
fn canonical_query(url: &url::Url) -> String {
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(name, value)| (aws_encode(&name), aws_encode(&value)))
        .collect();
    pairs.sort();
    pairs
        .into_iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-encoding as AWS defines it: everything but the unreserved characters.
fn aws_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn hmac(key: &[u8], message: &[u8]) -> Vec<u8> {
    let mut mac = hmac::Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes a key of any size");
    mac.update(message);
    mac.finalize().into_bytes().to_vec()
}

/// The date-, region- and service-scoped key AWS derives before signing.
fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let key = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let key = hmac(&key, region.as_bytes());
    let key = hmac(&key, service.as_bytes());
    hmac(&key, b"aws4_request")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials() -> Credentials {
        // The keys from AWS's own signing test suite.
        Credentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: String::new(),
            region: "us-east-1".into(),
            service: "service".into(),
        }
    }

    /// `get-vanilla` from AWS's sigv4 test suite, whose expected Authorization header is
    /// published with it.
    #[test]
    fn matches_the_aws_test_suite() {
        let mut request = Request {
            method: "GET".into(),
            url: "https://example.amazonaws.com/".into(),
            headers: vec![("Host".into(), "example.amazonaws.com".into())],
            ..Default::default()
        };
        sign(&mut request, &credentials(), "20150830T123600Z").unwrap();
        let authorization = request
            .headers
            .iter()
            .find(|(name, _)| name == "Authorization")
            .map(|(_, value)| value.clone())
            .unwrap();
        assert!(
            authorization.contains("Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request"),
            "{authorization}"
        );
        assert!(
            authorization.contains("Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"),
            "the published signature for get-vanilla:\n{authorization}"
        );
    }

    #[test]
    fn signs_a_query_and_a_body() {
        let mut request = Request {
            method: "put".into(),
            url: "https://bucket.s3.example.com/path/to/file?b=2&a=1&x=a%20b".into(),
            headers: vec![("Content-Type".into(), "text/plain".into())],
            body: "hello".into(),
            ..Default::default()
        };
        let mut credentials = credentials();
        credentials.service = "s3".into();
        credentials.session_token = "session-1".into();
        sign(&mut request, &credentials, "20150830T123600Z").unwrap();
        let header = |request: &Request, name: &str| {
            request
                .headers
                .iter()
                .find(|(header, _)| header.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.clone())
                .unwrap_or_default()
        };
        assert_eq!(
            header(&request, "host"),
            "bucket.s3.example.com",
            "host comes from the URL"
        );
        assert_eq!(
            header(&request, "x-amz-content-sha256"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
            "the body is hashed"
        );
        assert_eq!(header(&request, "x-amz-security-token"), "session-1");
        let authorization = header(&request, "authorization");
        assert!(
            authorization
                .contains("SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date;x-amz-security-token"),
            "every header is signed, sorted: {authorization}"
        );
        assert!(authorization.contains("/s3/aws4_request"), "{authorization}");

        // Signing twice in a row gives the same answer: the headers it adds are replaced,
        // not stacked.
        let before = request.headers.len();
        sign(&mut request, &credentials, "20150830T123600Z").unwrap();
        assert_eq!(request.headers.len(), before);
        assert_eq!(header(&request, "authorization"), authorization);
    }

    #[test]
    fn formats_timestamps_and_needs_credentials() {
        let at = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_440_938_160);
        assert_eq!(timestamp(at), "20150830T123600Z");
        assert_eq!(
            timestamp(std::time::UNIX_EPOCH + std::time::Duration::from_secs(0)),
            "19700101T000000Z"
        );
        let mut request = Request {
            method: "GET".into(),
            url: "https://example.amazonaws.com/".into(),
            ..Default::default()
        };
        assert!(sign(&mut request, &Credentials::default(), "20150830T123600Z").is_err());
    }
}
