//! Spotting literal credentials in requests and moving them into secret variables.
//!
//! Hoisting replaces a literal value with a `{{name}}` placeholder and records the value
//! under that name in a secrets map, so only the placeholder is ever written to YAML.

use crate::encoding::base64_decode;
use crate::model::{Header, RequestFile, Variables, placeholder};

/// Replaces literal credentials in `request` (header values and URL query parameters) with
/// `{{name}}` placeholders. `secrets` is the name -> value map accumulated so far (e.g. across
/// one import, or a collection's existing secrets): an identical value reuses its existing name;
/// a different value for a taken name gets `_2`, `_3`… Returns the names newly added to `secrets`.
#[cfg(test)]
pub fn hoist_credentials(request: &mut RequestFile, secrets: &mut Variables) -> Vec<String> {
    hoist_credentials_with(request, secrets, &|_| false)
}

/// Like [`hoist_credentials`], but never picks a new name for which `is_reserved` returns true,
/// e.g. names already used by plain variables in the same scope.
pub fn hoist_credentials_with(
    request: &mut RequestFile,
    secrets: &mut Variables,
    is_reserved: &dyn Fn(&str) -> bool,
) -> Vec<String> {
    let mut added = Vec::new();
    for index in 0..request.headers.len() {
        if let Some((name, is_new)) = hoist_header_inner(request, index, secrets, is_reserved)
            && is_new
        {
            added.push(name);
        }
    }
    added.extend(hoist_query(request, secrets, is_reserved));
    added
}

/// Moves a request's auth credential (a password, token or key written literally) into
/// `secrets`, leaving a `{{placeholder}}` behind. Returns the secret's name when one was
/// added. Headers are handled by [`hoist_credentials_with`]; this is the auth row.
pub fn hoist_auth(
    request: &mut RequestFile,
    secrets: &mut Variables,
    is_reserved: &dyn Fn(&str) -> bool,
) -> Option<String> {
    let value = request.auth.literal_credential()?.to_string();
    // The same credential twice gets one secret, as it does for headers.
    if let Some((name, _)) = secrets.iter().find(|(_, existing)| **existing == value) {
        let name = name.clone();
        request.auth.set_credential(format!("{{{{{name}}}}}"));
        return None;
    }
    let name = unique_name(&request.auth.credential_name(), |candidate| {
        secrets.contains_key(candidate) || is_reserved(candidate)
    });
    request.auth.set_credential(format!("{{{{{name}}}}}"));
    secrets.insert(name.clone(), value);
    Some(name)
}

/// Hoists just one header (by index) — used by the request editor's "Move to secret" button.
/// Returns the secret name now referenced by the header (newly added, or an existing name whose
/// value was identical), or None if that header holds no literal credential.
pub fn hoist_header(request: &mut RequestFile, index: usize, secrets: &mut Variables) -> Option<String> {
    hoist_header_inner(request, index, secrets, &|_| false).map(|(name, _)| name)
}

/// True when a header's value is a literal credential worth moving into a secret.
pub fn is_literal_credential(header: &Header) -> bool {
    plan_header(header).is_some()
}

/// True when a variable *name* suggests a credential, matching on word parts so that
/// `base_url` and `author_id` are not flagged.
pub fn looks_sensitive_name(name: &str) -> bool {
    let parts = word_parts(name);
    let has = |word: &str| parts.iter().any(|p| p == word);
    const WORDS: &[&str] = &[
        "token",
        "secret",
        "password",
        "passwd",
        "pwd",
        "passphrase",
        "auth",
        "authorization",
        "bearer",
        "cookie",
        "session",
        "apikey",
        "credential",
        "credentials",
        "privatekey",
        "accesskey",
        "signature",
    ];
    const SUFFIXES: &[&str] = &["token", "secret", "password", "passwd", "apikey"];
    WORDS.iter().any(|w| has(w))
        || parts
            .iter()
            .any(|p| SUFFIXES.iter().any(|s| p.len() > s.len() && p.ends_with(s)))
        || (has("key") && (has("api") || has("private") || has("access") || has("secret")))
}

/// Header value split into the part that stays and the part that becomes a secret.
struct HeaderPlan {
    prefix: String,
    value: String,
    base_name: String,
}

fn plan_header(header: &Header) -> Option<HeaderPlan> {
    let value = header.value.trim();
    if value.is_empty() || value.contains("{{") {
        return None;
    }
    let lower = header.name.trim().to_ascii_lowercase();
    match lower.as_str() {
        "authorization" | "proxy-authorization" => {
            let proxy = lower.starts_with("proxy");
            let other = if proxy { "proxy_authorization" } else { "authorization" };
            match value.split_once(char::is_whitespace) {
                Some((scheme, rest))
                    if !rest.trim().is_empty() && scheme.chars().all(|c| c.is_ascii_alphanumeric()) =>
                {
                    let rest = rest.trim();
                    let base_name = match scheme.to_ascii_lowercase().as_str() {
                        "bearer" => "bearer_token",
                        "basic" => {
                            // Basic credentials built from `{{user}}:{{pass}}` are already templated.
                            if base64_decode(rest).is_some_and(|d| String::from_utf8_lossy(&d).contains("{{")) {
                                return None;
                            }
                            "basic_auth"
                        }
                        _ => other,
                    };
                    Some(HeaderPlan {
                        prefix: format!("{scheme} "),
                        value: rest.into(),
                        base_name: base_name.into(),
                    })
                }
                _ => Some(HeaderPlan {
                    prefix: String::new(),
                    value: value.into(),
                    base_name: other.into(),
                }),
            }
        }
        "cookie" => Some(HeaderPlan {
            prefix: String::new(),
            value: value.into(),
            base_name: "cookie".into(),
        }),
        _ if is_sensitive_header(&header.name) => Some(HeaderPlan {
            prefix: String::new(),
            value: value.into(),
            base_name: header_secret_name(&header.name),
        }),
        _ => None,
    }
}

fn is_sensitive_header(name: &str) -> bool {
    let stripped = strip_x_prefix(name);
    looks_sensitive_name(stripped) || word_parts(stripped) == ["key"]
}

fn strip_x_prefix(name: &str) -> &str {
    let name = name.trim();
    match name.get(..2) {
        Some(prefix) if prefix.eq_ignore_ascii_case("x-") => &name[2..],
        _ => name,
    }
}

/// `X-API-Key` becomes `api_key`, `X-Auth-Token` becomes `auth_token`.
fn header_secret_name(name: &str) -> String {
    snake_case(strip_x_prefix(name))
}

fn hoist_header_inner(
    request: &mut RequestFile,
    index: usize,
    secrets: &mut Variables,
    is_reserved: &dyn Fn(&str) -> bool,
) -> Option<(String, bool)> {
    let header = request.headers.get(index)?;
    let plan = plan_header(header)?;
    let (name, is_new) = allocate(&plan.base_name, &plan.value, secrets, is_reserved);
    request.headers[index].value = format!("{}{}", plan.prefix, placeholder(&name));
    Some((name, is_new))
}

const SENSITIVE_QUERY_PARAMS: &[&str] = &[
    "api_key",
    "apikey",
    "api-key",
    "key",
    "access_token",
    "token",
    "client_secret",
    "signature",
    "sig",
];

fn hoist_query(request: &mut RequestFile, secrets: &mut Variables, is_reserved: &dyn Fn(&str) -> bool) -> Vec<String> {
    let url = request.url.clone();
    let Some(query_start) = url.find('?') else {
        return Vec::new();
    };
    let query_end = url[query_start..].find('#').map_or(url.len(), |i| query_start + i);
    let query = &url[query_start + 1..query_end];

    let mut added = Vec::new();
    let mut changed = false;
    let params: Vec<String> = query
        .split('&')
        .map(|param| {
            let Some((key, value)) = param.split_once('=') else {
                return param.to_string();
            };
            let lower = key.to_ascii_lowercase();
            let sensitive = SENSITIVE_QUERY_PARAMS.contains(&lower.as_str()) || looks_sensitive_name(key);
            if !sensitive || value.is_empty() || value.contains("{{") || key.contains("{{") {
                return param.to_string();
            }
            let (name, is_new) = allocate(&snake_case(key), value, secrets, is_reserved);
            if is_new {
                added.push(name.clone());
            }
            changed = true;
            format!("{key}={}", placeholder(&name))
        })
        .collect();

    if changed {
        request.url = format!("{}?{}{}", &url[..query_start], params.join("&"), &url[query_end..]);
    }
    added
}

/// Picks the name for `value`: an existing name holding the same value, else `base`, `base_2`…
/// Returns the name and whether it was newly inserted.
fn allocate(base: &str, value: &str, secrets: &mut Variables, is_reserved: &dyn Fn(&str) -> bool) -> (String, bool) {
    if let Some((name, _)) = secrets.iter().find(|(_, v)| v.as_str() == value) {
        return (name.clone(), false);
    }
    let base = if base.is_empty() { "secret" } else { base };
    let name = unique_name(base, |name| secrets.contains_key(name) || is_reserved(name));
    secrets.insert(name.clone(), value.to_string());
    (name, true)
}

/// `base`, or `base_2`, `base_3`… — the first that `taken` rejects.
pub fn unique_name(base: &str, taken: impl Fn(&str) -> bool) -> String {
    let mut name = base.to_string();
    let mut n = 2;
    while taken(&name) {
        name = format!("{base}_{n}");
        n += 1;
    }
    name
}

/// A secrets map holding only `names`, with a value that never matches a real credential, so
/// hoisting picks fresh names instead of reusing these.
pub fn reserved_names(names: impl IntoIterator<Item = String>) -> Variables {
    names.into_iter().map(|name| (name, "\0existing".to_string())).collect()
}

/// Lower-case word parts, splitting on non-alphanumerics and camelCase boundaries.
fn word_parts(name: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut prev_lower = false;
    for c in name.chars() {
        if !c.is_alphanumeric() {
            if !current.is_empty() {
                parts.push(std::mem::take(&mut current));
            }
            prev_lower = false;
            continue;
        }
        if c.is_uppercase() && prev_lower && !current.is_empty() {
            parts.push(std::mem::take(&mut current));
        }
        prev_lower = c.is_lowercase() || c.is_ascii_digit();
        current.extend(c.to_lowercase());
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

fn snake_case(name: &str) -> String {
    word_parts(name).join("_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::curl;

    fn header(name: &str, value: &str) -> Header {
        Header::new(name, value)
    }

    fn request_with(headers: Vec<Header>, url: &str) -> RequestFile {
        let mut request = RequestFile::new("r");
        request.headers = headers;
        request.url = url.into();
        request
    }

    fn value<'a>(request: &'a RequestFile, name: &str) -> &'a str {
        &request
            .headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(name))
            .unwrap()
            .value
    }

    #[test]
    fn sensitive_names() {
        for name in [
            "token",
            "api_token",
            "apiKey",
            "api-key",
            "API_KEY",
            "apikey",
            "client_secret",
            "password",
            "db_passwd",
            "auth",
            "AuthToken",
            "bearer",
            "session_id",
            "cookie",
            "private_key",
            "accessKey",
            "refreshtoken",
            "X-Auth-Token",
        ] {
            assert!(looks_sensitive_name(name), "{name} should be sensitive");
        }
        for name in [
            "base_url",
            "author_id",
            "Content-Type",
            "Accept",
            "User-Agent",
            "sort_key",
            "keyword",
            "tokenizer_mode",
            "page",
        ] {
            assert!(!looks_sensitive_name(name), "{name} should not be sensitive");
        }
    }

    #[test]
    fn hoists_chrome_copy_as_curl() {
        let command = r#"curl 'https://api.example.com/v1/users?page=2&api_key=k-123' \
  -H 'accept: application/json, text/plain, */*' \
  -H 'authorization: Bearer eyJhbGciOi.payload.sig' \
  -H 'content-type: application/json' \
  -b 'session=abc123; theme=dark' \
  -H 'user-agent: Mozilla/5.0 (X11; Linux x86_64)' \
  -H 'x-api-key: key-999' \
  --data-raw '{"a":1}'"#;
        let mut request = curl::parse(command).unwrap();
        let mut secrets = Variables::new();
        let added = hoist_credentials(&mut request, &mut secrets);

        assert_eq!(added, ["bearer_token", "cookie", "api_key", "api_key_2"]);
        assert_eq!(value(&request, "authorization"), "Bearer {{bearer_token}}");
        assert_eq!(value(&request, "Cookie"), "{{cookie}}");
        assert_eq!(value(&request, "x-api-key"), "{{api_key}}");
        assert_eq!(
            request.url,
            "https://api.example.com/v1/users?page=2&api_key={{api_key_2}}"
        );
        assert_eq!(value(&request, "accept"), "application/json, text/plain, */*");
        assert_eq!(value(&request, "content-type"), "application/json");
        assert_eq!(value(&request, "user-agent"), "Mozilla/5.0 (X11; Linux x86_64)");
        assert_eq!(secrets["bearer_token"], "eyJhbGciOi.payload.sig");
        assert_eq!(secrets["cookie"], "session=abc123; theme=dark");
        assert_eq!(secrets["api_key"], "key-999");
        assert_eq!(secrets["api_key_2"], "k-123");
    }

    #[test]
    fn dedupes_identical_values_and_suffixes_different_ones() {
        let mut secrets = Variables::new();
        let mut a = request_with(vec![header("Authorization", "Bearer same")], "");
        let mut b = request_with(vec![header("Authorization", "Bearer same")], "");
        let mut c = request_with(vec![header("Authorization", "Bearer other")], "");
        assert_eq!(hoist_credentials(&mut a, &mut secrets), ["bearer_token"]);
        assert!(
            hoist_credentials(&mut b, &mut secrets).is_empty(),
            "identical value reuses the name"
        );
        assert_eq!(value(&b, "Authorization"), "Bearer {{bearer_token}}");
        assert_eq!(hoist_credentials(&mut c, &mut secrets), ["bearer_token_2"]);
        assert_eq!(secrets.len(), 2);
    }

    #[test]
    fn authorization_schemes() {
        let mut secrets = Variables::new();
        let mut request = request_with(
            vec![
                header("Authorization", "Basic YWRtaW46c2VjcmV0"),
                header("Proxy-Authorization", "Token abc"),
                header("Authorization", "rawvalue"),
                header("Authorization", "Basic e3t1c2VyfX06e3twYXNzfX0="), // base64("{{user}}:{{pass}}")
            ],
            "",
        );
        hoist_credentials(&mut request, &mut secrets);
        assert_eq!(request.headers[0].value, "Basic {{basic_auth}}");
        assert_eq!(request.headers[1].value, "Token {{proxy_authorization}}");
        assert_eq!(request.headers[2].value, "{{authorization}}");
        assert_eq!(
            request.headers[3].value, "Basic e3t1c2VyfX06e3twYXNzfX0=",
            "templated basic auth left alone"
        );
        assert_eq!(secrets["basic_auth"], "YWRtaW46c2VjcmV0");
    }

    #[test]
    fn skips_placeholders_empty_and_harmless_headers_but_not_disabled_ones() {
        let mut disabled = header("X-Auth-Token", "t0k");
        disabled.enabled = false;
        let mut request = request_with(
            vec![
                header("Authorization", "Bearer {{token}}"),
                header("X-Api-Key", ""),
                header("Content-Type", "application/json"),
                header("Idempotency-Key", "42"),
                disabled,
            ],
            "{{base_url}}/x?token={{token}}&author_id=7&key=",
        );
        let mut secrets = Variables::new();
        assert_eq!(hoist_credentials(&mut request, &mut secrets), ["auth_token"]);
        assert_eq!(request.headers[4].value, "{{auth_token}}");
        assert_eq!(request.url, "{{base_url}}/x?token={{token}}&author_id=7&key=");
        assert!(!is_literal_credential(&request.headers[0]));
        assert!(!is_literal_credential(&header("Idempotency-Key", "42")));
        assert!(is_literal_credential(&header("X-Key", "abc")));
    }

    #[test]
    fn query_keeps_order_encoding_and_fragment() {
        let mut request = request_with(vec![], "https://x.test/p?a=1&Access_Token=a%2Fb&z=2#frag");
        let mut secrets = Variables::new();
        assert_eq!(hoist_credentials(&mut request, &mut secrets), ["access_token"]);
        assert_eq!(
            request.url,
            "https://x.test/p?a=1&Access_Token={{access_token}}&z=2#frag"
        );
        assert_eq!(secrets["access_token"], "a%2Fb");
    }

    #[test]
    fn hoist_single_header_and_reserved_names() {
        let mut request = request_with(vec![header("Accept", "x"), header("X-API-Key", "abc")], "");
        let mut secrets = Variables::new();
        assert_eq!(hoist_header(&mut request, 0, &mut secrets), None);
        assert_eq!(hoist_header(&mut request, 5, &mut secrets), None);
        assert_eq!(hoist_header(&mut request, 1, &mut secrets).as_deref(), Some("api_key"));
        assert_eq!(request.headers[1].value, "{{api_key}}");

        let mut other = request_with(vec![header("X-API-Key", "different")], "");
        let added = hoist_credentials_with(&mut other, &mut secrets, &|n| n == "api_key_2");
        assert_eq!(added, ["api_key_3"]);
    }
}
