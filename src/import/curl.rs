//! Importing a request from a curl command line, as copied from browser devtools or API docs.

use anyhow::{Result, bail};

use crate::encoding::{base64_encode, percent_encode};
use crate::model::{Body, BodyKind, Header, RequestFile};

/// Parses a curl command line (as copied from browser devtools / docs) into a request.
pub fn parse(command: &str) -> Result<RequestFile> {
    let tokens = tokenize(command)?;
    let mut tokens = tokens.into_iter();

    match tokens.next() {
        Some(first) if first == "curl" || first.ends_with("/curl") || first == "curl.exe" => {}
        _ => bail!("Not a curl command: it should start with `curl`"),
    }

    let mut parsed = Parsed::default();
    while let Some(token) = tokens.next() {
        if token == "--" {
            parsed.urls.extend(tokens.by_ref());
            break;
        }
        if let Some(long) = token.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((name, value)) => (name, Some(value.to_string())),
                None => (long, None),
            };
            if long_takes_value(name) {
                let Some(value) = inline.or_else(|| tokens.next()) else {
                    bail!("`--{name}` is missing its value");
                };
                parsed.apply(Flag::Long(name), value);
            } else {
                parsed.apply_switch(Flag::Long(name));
            }
        } else if token.len() > 1 && token.starts_with('-') {
            // Short flags can be bundled (`-sSL`) and can carry an attached value (`-XPOST`).
            let chars: Vec<char> = token[1..].chars().collect();
            let mut i = 0;
            while i < chars.len() {
                let c = chars[i];
                if short_takes_value(c) {
                    let rest: String = chars[i + 1..].iter().collect();
                    let value = if rest.is_empty() { tokens.next() } else { Some(rest) };
                    let Some(value) = value else {
                        bail!("`-{c}` is missing its value");
                    };
                    parsed.apply(Flag::Short(c), value);
                    break;
                }
                parsed.apply_switch(Flag::Short(c));
                i += 1;
            }
        } else {
            parsed.urls.push(token);
        }
    }

    parsed.into_request()
}

enum Flag<'a> {
    Short(char),
    Long(&'a str),
}

#[derive(Default)]
struct Parsed {
    urls: Vec<String>,
    method: Option<String>,
    head: bool,
    get: bool,
    headers: Vec<Header>,
    data: Vec<String>,
    json: Option<String>,
}

impl Parsed {
    fn apply(&mut self, flag: Flag, value: String) {
        use Flag::{Long, Short};
        match flag {
            Short('X') | Long("request") => self.method = Some(value.to_uppercase()),
            Short('H') | Long("header") => self.add_header_line(&value),
            Short('d') | Long("data" | "data-raw" | "data-binary" | "data-ascii") => self.data.push(value),
            Long("data-urlencode") => self.data.push(urlencode_data(&value)),
            Long("json") => self.json.get_or_insert_with(String::new).push_str(&value),
            Short('u') | Long("user") => {
                let credentials = if value.contains(':') {
                    value
                } else {
                    format!("{value}:")
                };
                self.set_header(
                    "Authorization",
                    format!("Basic {}", base64_encode(credentials.as_bytes())),
                );
            }
            Long("oauth2-bearer") => self.set_header("Authorization", format!("Bearer {value}")),
            Short('A') | Long("user-agent") => self.set_header("User-Agent", value),
            Short('e') | Long("referer") => self.set_header("Referer", value),
            Short('b') | Long("cookie") => self.set_header("Cookie", value),
            Long("url") => self.urls.push(value),
            // Everything else that takes a value (output files, timeouts, proxies, ...) is
            // irrelevant to the request itself and is dropped.
            _ => {}
        }
    }

    fn apply_switch(&mut self, flag: Flag) {
        match flag {
            Flag::Short('G') | Flag::Long("get") => self.get = true,
            Flag::Short('I') | Flag::Long("head") => self.head = true,
            // --compressed, -s, -S, -L, -k, -i, -v, --location, --insecure and friends.
            _ => {}
        }
    }

    fn add_header_line(&mut self, line: &str) {
        if let Some((name, value)) = line.split_once(':') {
            let value = value.trim();
            // `-H "Name:"` tells curl to remove a header; there is nothing to import.
            if !value.is_empty() {
                self.headers.push(Header {
                    name: name.trim().to_string(),
                    value: value.to_string(),
                    enabled: true,
                });
            }
        } else if let Some(name) = line.trim().strip_suffix(';') {
            // `-H "Name;"` sends the header with an empty value.
            self.headers.push(Header::new(name.trim(), ""));
        }
    }

    fn has_header(&self, name: &str) -> bool {
        self.headers.iter().any(|h| h.name.eq_ignore_ascii_case(name))
    }

    fn set_header(&mut self, name: &str, value: String) {
        self.headers.retain(|h| !h.name.eq_ignore_ascii_case(name));
        self.headers.push(Header::new(name, value));
    }

    fn into_request(mut self) -> Result<RequestFile> {
        let Some(mut url) = self.urls.first().cloned() else {
            bail!("The curl command has no URL");
        };

        let mut body = None;
        if let Some(json) = self.json.take() {
            if !self.has_header("Content-Type") {
                self.headers.push(header("Content-Type", "application/json"));
            }
            if !self.has_header("Accept") {
                self.headers.push(header("Accept", "application/json"));
            }
            body = Some(json);
        }
        if !self.data.is_empty() {
            let joined = self.data.join("&");
            if self.get {
                url.push(if url.contains('?') { '&' } else { '?' });
                url.push_str(&joined);
            } else {
                body = Some(match body {
                    Some(json) => json + &joined,
                    None => joined,
                });
            }
        }

        let method = match self.method {
            Some(method) => method,
            None if self.head => "HEAD".into(),
            None if body.is_some() => "POST".into(),
            None => "GET".into(),
        };

        let body = body.map(|content| {
            let content_type = self
                .headers
                .iter()
                .find(|h| h.name.eq_ignore_ascii_case("Content-Type"))
                .map(|h| h.value.as_str());
            let kind = match content_type {
                Some(content_type) => BodyKind::from_content_type(content_type),
                None if serde_json::from_str::<serde_json::Value>(&content).is_ok() => BodyKind::Json,
                None => BodyKind::Text,
            };
            Body { kind, content }
        });

        Ok(RequestFile {
            name: format!("{method} {}", url_path(&url)),
            method,
            url,
            headers: self.headers,
            body,
            order: None,
            messages: Vec::new(),
            graphql: None,
        })
    }
}

fn header(name: &str, value: &str) -> Header {
    Header::new(name, value)
}

fn short_takes_value(c: char) -> bool {
    matches!(
        c,
        'X' | 'H'
            | 'd'
            | 'u'
            | 'A'
            | 'e'
            | 'b'
            | 'o'
            | 'F'
            | 'T'
            | 'w'
            | 'm'
            | 'x'
            | 'E'
            | 'r'
            | 'z'
            | 'K'
            | 'c'
            | 'D'
            | 'C'
            | 'y'
            | 'Y'
            | 'U'
            | 'Q'
            | 't'
    )
}

fn long_takes_value(name: &str) -> bool {
    matches!(
        name,
        "request"
            | "header"
            | "data"
            | "data-raw"
            | "data-binary"
            | "data-ascii"
            | "data-urlencode"
            | "json"
            | "user"
            | "user-agent"
            | "referer"
            | "cookie"
            | "url"
            | "oauth2-bearer"
            | "output"
            | "form"
            | "form-string"
            | "upload-file"
            | "write-out"
            | "max-time"
            | "connect-timeout"
            | "proxy"
            | "proxy-user"
            | "cert"
            | "key"
            | "cacert"
            | "capath"
            | "range"
            | "cookie-jar"
            | "dump-header"
            | "config"
            | "retry"
            | "retry-delay"
            | "retry-max-time"
            | "resolve"
            | "connect-to"
            | "interface"
            | "limit-rate"
            | "time-cond"
            | "max-redirs"
            | "max-filesize"
            | "quote"
            | "telnet-option"
            | "cert-type"
            | "key-type"
            | "pass"
            | "ciphers"
            | "aws-sigv4"
            | "variable"
            | "expand-url"
            | "unix-socket"
            | "abstract-unix-socket"
            | "local-port"
            | "dns-servers"
            | "noproxy"
            | "request-target"
            | "header-file"
            | "trace"
            | "trace-ascii"
            | "stderr"
    )
}

/// `--data-urlencode` forms: `content`, `=content`, `name=content`. `@file` forms are kept verbatim.
fn urlencode_data(value: &str) -> String {
    if value.starts_with('@') || value.contains("=@") {
        return value.to_string();
    }
    match value.split_once('=') {
        Some(("", content)) => percent_encode(content),
        Some((name, content)) => format!("{name}={}", percent_encode(content)),
        None => percent_encode(value),
    }
}

/// The path part of a URL for naming requests: `https://x.test/users?id=1` gives `/users`.
fn url_path(url: &str) -> &str {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let path = after_scheme.find('/').map_or("/", |i| &after_scheme[i..]);
    let end = path.find(['?', '#']).unwrap_or(path.len());
    &path[..end]
}

/// Splits a command line the way a POSIX shell would, supporting `'...'`, `"..."`, `$'...'`,
/// backslash escapes, and `\` / `^` line continuations (bash and Windows cmd copies).
fn tokenize(input: &str) -> Result<Vec<String>> {
    let chars: Vec<char> = input.trim().trim_start_matches("$ ").chars().collect();
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_token = false;
    let mut i = 0;

    let is_newline_at =
        |i: usize| chars.get(i) == Some(&'\n') || (chars.get(i) == Some(&'\r') && chars.get(i + 1) == Some(&'\n'));
    let newline_len = |i: usize| if chars.get(i) == Some(&'\r') { 2 } else { 1 };

    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' | '\n' | '\r' => {
                if in_token {
                    tokens.push(std::mem::take(&mut current));
                    in_token = false;
                }
                i += 1;
            }
            '\\' | '^' if is_newline_at(i + 1) => i += 1 + newline_len(i + 1),
            '\\' => {
                if let Some(&next) = chars.get(i + 1) {
                    current.push(next);
                }
                in_token = true;
                i += 2;
            }
            '\'' => {
                let Some(len) = chars[i + 1..].iter().position(|&c| c == '\'') else {
                    bail!("Unterminated single quote");
                };
                current.extend(&chars[i + 1..i + 1 + len]);
                in_token = true;
                i += len + 2;
            }
            '$' if chars.get(i + 1) == Some(&'\'') => {
                i = ansi_c_quoted(&chars, i + 2, &mut current)?;
                in_token = true;
            }
            '"' => {
                i += 1;
                loop {
                    match chars.get(i) {
                        None => bail!("Unterminated double quote"),
                        Some('"') => break,
                        Some('\\') if matches!(chars.get(i + 1), Some('"' | '\\' | '$' | '`')) => {
                            current.push(chars[i + 1]);
                            i += 2;
                        }
                        Some('\\') if is_newline_at(i + 1) => i += 1 + newline_len(i + 1),
                        Some(&c) => {
                            current.push(c);
                            i += 1;
                        }
                    }
                }
                in_token = true;
                i += 1;
            }
            _ => {
                current.push(c);
                in_token = true;
                i += 1;
            }
        }
    }
    if in_token {
        tokens.push(current);
    }
    Ok(tokens)
}

/// Decodes the body of a `$'...'` string starting at `i`; returns the index after the closing quote.
fn ansi_c_quoted(chars: &[char], mut i: usize, out: &mut String) -> Result<usize> {
    let hex = |digits: &[char]| u32::from_str_radix(&digits.iter().collect::<String>(), 16).ok();
    loop {
        match chars.get(i) {
            None => bail!("Unterminated $'...' string"),
            Some('\'') => return Ok(i + 1),
            Some('\\') => {
                let Some(&escape) = chars.get(i + 1) else {
                    bail!("Unterminated $'...' string");
                };
                i += 2;
                match escape {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    'e' | 'E' => out.push('\u{1b}'),
                    '0' => out.push('\0'),
                    'x' | 'u' | 'U' => {
                        let max = match escape {
                            'x' => 2,
                            'u' => 4,
                            _ => 8,
                        };
                        let len = chars[i..]
                            .iter()
                            .take(max)
                            .take_while(|c| c.is_ascii_hexdigit())
                            .count();
                        match hex(&chars[i..i + len]).and_then(char::from_u32) {
                            Some(c) if len > 0 => out.push(c),
                            _ => {
                                out.push('\\');
                                out.push(escape);
                            }
                        }
                        i += len;
                    }
                    '\\' | '\'' | '"' | '?' => out.push(escape),
                    other => {
                        out.push('\\');
                        out.push(other);
                    }
                }
            }
            Some(&c) => {
                out.push(c);
                i += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_value<'a>(request: &'a RequestFile, name: &str) -> Option<&'a str> {
        request
            .headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(name))
            .map(|h| h.value.as_str())
    }

    #[test]
    fn parses_chrome_copy_as_curl() {
        let command = r#"curl 'https://api.example.com/v1/users?page=2' \
  -H 'accept: application/json, text/plain, */*' \
  -H 'accept-language: en-GB,en;q=0.9' \
  -H 'authorization: Bearer eyJhbGciOi.payload.sig' \
  -H 'content-type: application/json' \
  -b 'session=abc123; theme=dark' \
  -H 'origin: https://app.example.com' \
  -H 'sec-ch-ua: "Chromium";v="128", "Not;A=Brand";v="24"' \
  -H 'user-agent: Mozilla/5.0 (X11; Linux x86_64)' \
  --data-raw $'{"name":"O\'Brien","bio":"line1\\nline2"}' \
  --compressed"#;
        let request = parse(command).unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.url, "https://api.example.com/v1/users?page=2");
        assert_eq!(request.name, "POST /v1/users");
        assert_eq!(header_value(&request, "Cookie"), Some("session=abc123; theme=dark"));
        assert_eq!(
            header_value(&request, "sec-ch-ua"),
            Some(r#""Chromium";v="128", "Not;A=Brand";v="24""#)
        );
        assert_eq!(request.headers.len(), 8);
        let body = request.body.unwrap();
        assert_eq!(body.kind, BodyKind::Json);
        assert_eq!(body.content, r#"{"name":"O'Brien","bio":"line1\nline2"}"#);
    }

    #[test]
    fn parses_docs_style_command() {
        let request = parse(
            "curl -sSL -X put \"https://x.test/items/{{id}}\" -u admin:secret -A my-agent -d a=1 -d 'b=two words' -o out.json",
        )
        .unwrap();
        assert_eq!(request.method, "PUT");
        assert_eq!(request.url, "https://x.test/items/{{id}}");
        assert_eq!(header_value(&request, "Authorization"), Some("Basic YWRtaW46c2VjcmV0"));
        assert_eq!(header_value(&request, "User-Agent"), Some("my-agent"));
        let body = request.body.unwrap();
        assert_eq!(body.content, "a=1&b=two words");
        assert_eq!(body.kind, BodyKind::Text);
    }

    #[test]
    fn json_flag_sets_headers_and_post() {
        let request = parse(r#"curl --json '{"ok":true}' --url=https://x.test/"#).unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(header_value(&request, "Content-Type"), Some("application/json"));
        assert_eq!(header_value(&request, "Accept"), Some("application/json"));
        assert_eq!(request.body.unwrap().kind, BodyKind::Json);
        assert_eq!(request.name, "POST /");
    }

    #[test]
    fn get_flag_moves_data_to_query() {
        let request = parse("curl -G https://x.test/search?lang=en --data-urlencode 'q=rust gpui' -d page=2").unwrap();
        assert_eq!(request.method, "GET");
        assert_eq!(request.url, "https://x.test/search?lang=en&q=rust%20gpui&page=2");
        assert!(request.body.is_none());
    }

    #[test]
    fn windows_cmd_continuations_and_attached_values() {
        let request = parse("curl \"https://x.test/a\" ^\r\n  -XDELETE ^\r\n  -H \"Accept: */*\"").unwrap();
        assert_eq!(request.method, "DELETE");
        assert_eq!(header_value(&request, "Accept"), Some("*/*"));
    }

    #[test]
    fn head_and_header_edge_cases() {
        let request = parse("curl -I https://x.test -H 'X-Empty;' -H 'Accept:'").unwrap();
        assert_eq!(request.method, "HEAD");
        assert_eq!(request.headers.len(), 1);
        assert_eq!(header_value(&request, "X-Empty"), Some(""));
    }

    #[test]
    fn rejects_non_curl_and_missing_url() {
        assert!(
            parse("wget https://x.test")
                .unwrap_err()
                .to_string()
                .contains("Not a curl command")
        );
        assert!(
            parse("curl -H 'Accept: */*'")
                .unwrap_err()
                .to_string()
                .contains("no URL")
        );
        assert!(parse("curl 'https://x.test").is_err());
    }

    #[test]
    fn base64_and_percent_encoding() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(percent_encode("a b&c={{token}}/é"), "a%20b%26c%3D{{token}}%2F%C3%A9");
    }
}
