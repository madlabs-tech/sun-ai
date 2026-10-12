//! Helpers for mods that call a third-party JSON API over HTTP (feature
//! `http`): a configured [`agent`], [`send`] that maps HTTP failures onto
//! protocol error codes, URL helpers, and a [`mock`] server for tests.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::Value;
pub use ureq::Agent;
use ureq::SendBody;

use crate::{RpcError, code};

/// An HTTP agent that returns 4xx/5xx as responses (not errors) and gives up
/// after 30 s.
pub fn agent() -> Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .new_agent()
}

/// Accepts an https URL, or plain http to loopback only (tests, local
/// proxies), because the token goes out on every request. Strips a trailing `/`.
pub fn check_api_url(url: &str) -> Result<String, RpcError> {
    let url = url.trim_end_matches('/');
    let loopback = url.strip_prefix("http://").is_some_and(|rest| {
        let host = rest.split(['/', ':']).next().unwrap_or_default();
        matches!(host, "localhost" | "127.0.0.1")
    });
    match url.starts_with("https://") || loopback {
        true => Ok(url.to_owned()),
        false => Err(RpcError::invalid(format!(
            "api_url must be https, got {url:?}"
        ))),
    }
}

/// The per-call `credentials: {token, api_url}` the kernel adds to a git
/// host mod's call params (a workspace's connection), taken out of `params`
/// so the action's own fields stay strict. `None` without them: the mod's
/// own config or env applies. `api_url` goes through [`check_api_url`].
pub fn take_credentials(params: &mut Value) -> Result<Option<(String, String)>, RpcError> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Credentials {
        token: String,
        api_url: String,
    }
    let Some(given) = params.as_object_mut().and_then(|p| p.remove("credentials")) else {
        return Ok(None);
    };
    let c: Credentials = serde_json::from_value(given)
        .map_err(|_| RpcError::invalid("credentials must be {token, api_url}"))?;
    if c.token.is_empty() {
        return Err(RpcError::invalid("credentials.token is empty"));
    }
    Ok(Some((c.token, check_api_url(&c.api_url)?)))
}

/// Accepts an RFC 3339 time such as a `since` cursor (`2026-10-07T12:00:00Z`)
/// before it goes into a provider query, so it can't smuggle query syntax.
pub fn check_time(t: &str) -> Result<&str, RpcError> {
    let b = t.as_bytes();
    let ok = (20..=35).contains(&b.len())
        && b[..10]
            .iter()
            .enumerate()
            .all(|(i, c)| matches!(i, 4 | 7) == (*c == b'-') && (c.is_ascii_digit() || *c == b'-'))
        && b[10] == b'T'
        && b[11..].iter().all(|c| b"0123456789:.Z+-".contains(c));
    match ok {
        true => Ok(t),
        false => Err(RpcError::invalid(format!(
            "since must be an RFC 3339 time, got {t:?}"
        ))),
    }
}

/// A response with its body read.
#[derive(Debug)]
pub struct Response {
    /// `"METHOD /path"` without the query, for error messages.
    pub what: String,
    /// HTTP status.
    pub status: u16,
    /// Response headers.
    pub headers: ureq::http::HeaderMap,
    /// Body text.
    pub body: String,
}

impl Response {
    /// The header `name` as text.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    /// Parses a successful JSON body. A status ≥ 400 is
    /// [`UNAVAILABLE`](code::UNAVAILABLE) when it is 429, 5xx or `retry` is
    /// set (e.g. a rate-limit 403), else [`FAILED`](code::FAILED); the message
    /// carries the body's `message` or `error` field and `data.http_status`
    /// the status ([`RpcError::with_http_status`]).
    pub fn json<T: DeserializeOwned>(self, retry: bool) -> Result<T, RpcError> {
        let what = self.what;
        if self.status >= 400 {
            let message = serde_json::from_str::<Value>(&self.body)
                .ok()
                .and_then(|v| v.get("message").or_else(|| v.get("error")).cloned())
                .map(|m| m.as_str().map_or_else(|| m.to_string(), str::to_owned))
                .unwrap_or_default();
            let code = match retry || self.status == 429 || self.status >= 500 {
                true => code::UNAVAILABLE,
                false => code::FAILED,
            };
            let status = self.status;
            return Err(
                RpcError::new(code, format!("{what}: HTTP {status} {message}"))
                    .with_http_status(status),
            );
        }
        serde_json::from_str(&self.body)
            .map_err(|e| RpcError::failed(format!("{what}: unexpected response: {e}")))
    }
}

/// Sends one request to `base` + `path`. A JSON `body` sets `content-type`. Transport failures are
/// [`UNAVAILABLE`](code::UNAVAILABLE). Messages never include headers, so
/// tokens stay out of them.
pub fn send(
    agent: &Agent,
    method: &str,
    base: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&Value>,
) -> Result<Response, RpcError> {
    let body = body.map(|b| {
        (
            "application/json; charset=utf-8",
            b.to_string().into_bytes(),
        )
    });
    send_raw(agent, method, base, path, headers, body)
}

/// Like [`send`], with a raw `(content_type, bytes)` body, e.g. a multipart
/// upload.
pub fn send_raw(
    agent: &Agent,
    method: &str,
    base: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<(&str, Vec<u8>)>,
) -> Result<Response, RpcError> {
    let what = format!("{method} {}", path.split('?').next().unwrap_or_default());
    let mut req = ureq::http::Request::builder()
        .method(method)
        .uri(format!("{base}{path}"));
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    let sent = match body {
        None => req.body(SendBody::none()).map(|r| agent.run(r)),
        Some((content_type, bytes)) => req
            .header("content-type", content_type)
            .body(bytes)
            .map(|r| agent.run(r)),
    };
    let mut res = sent
        .map_err(|e| RpcError::failed(format!("{what}: {e}")))?
        .map_err(|e| RpcError::unavailable(format!("{what}: {e}")))?;
    let body = res
        .body_mut()
        .read_to_string()
        .map_err(|e| RpcError::unavailable(format!("{what}: {e}")))?;
    Ok(Response {
        what,
        status: res.status().as_u16(),
        headers: res.headers().clone(),
        body,
    })
}

/// Percent-encodes a path segment or query value.
pub fn encode(s: &str) -> String {
    s.bytes()
        .map(
            |b| match b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                true => (b as char).to_string(),
                false => format!("%{b:02X}"),
            },
        )
        .collect()
}

/// A hidden HTML comment that marks remote text (a PR comment, an MR note)
/// as applied for `idem_key`, so a replayed call can find it.
pub fn idem_marker(key: &str) -> Result<String, RpcError> {
    let ok = (1..=128).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.:-".contains(&b));
    match ok {
        true => Ok(format!("<!-- sun-idem:{key} -->")),
        false => Err(RpcError::invalid(
            "idem_key must be 1-128 chars of [A-Za-z0-9_.:-]",
        )),
    }
}

/// The body of the one comment an `attach_to_pr` action posts: `title`, the
/// verdict, the device, and each `(alt, url)` image inline. Shared by the
/// GitHub and GitLab mods so their comments match.
pub fn attachment_comment(
    title: &str,
    passed: bool,
    device: &str,
    images: &[(String, String)],
) -> String {
    let verdict = if passed { "passed" } else { "failed" };
    let mut body = format!("### {title}\n\n**Result:** {verdict} · **Device:** {device}\n");
    for (alt, url) in images {
        body.push_str(&format!("\n![{alt}]({url})\n"));
    }
    body
}

/// `set_status` params shared by the git-host mods (mod-github, mod-gitlab,
/// mod-gitea): one commit status on `sha`, e.g. Sun's `sun/merge-order`.
/// Setting what is already there is a no-op, so a replayed call is harmless.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitStatus {
    /// The host's repo path (checked by the mod).
    pub repo: String,
    /// Full or abbreviated commit SHA.
    pub sha: String,
    /// `pending`, `success` or `failure`.
    pub state: String,
    /// Status name, e.g. `sun/merge-order`.
    pub context: String,
    /// At most 140 chars.
    #[serde(default)]
    pub description: String,
    /// Link shown with the status.
    #[serde(default)]
    pub target_url: Option<String>,
}

impl CommitStatus {
    /// Parses and validates `params` (all but `repo`).
    pub fn parse(params: Value) -> Result<Self, RpcError> {
        let s: Self = crate::parse(params)?;
        let hex = (7..=64).contains(&s.sha.len()) && s.sha.bytes().all(|b| b.is_ascii_hexdigit());
        let problem = if !hex {
            "sha must be 7-64 hex digits"
        } else if !matches!(s.state.as_str(), "pending" | "success" | "failure") {
            "state must be pending, success or failure"
        } else if s.context.trim().is_empty()
            || s.context.len() > 255
            || s.context.chars().any(char::is_control)
        {
            "context must be 1-255 printable chars"
        } else if s.description.chars().count() > 140 {
            "description must be at most 140 chars"
        } else if s
            .target_url
            .as_deref()
            .is_some_and(|u| !u.starts_with("https://") && !u.starts_with("http://"))
        {
            "target_url must be http(s)"
        } else {
            return Ok(s);
        };
        Err(RpcError::invalid(problem))
    }

    /// Whether the host's current status for the context (`state` already
    /// mapped to `pending | success | failure`) is this one.
    pub fn matches(&self, state: &str, description: Option<&str>) -> bool {
        state == self.state && description.unwrap_or_default() == self.description
    }

    /// The action's result.
    pub fn result(&self, changed: bool) -> Value {
        serde_json::json!({
            "sha": self.sha, "state": self.state, "context": self.context, "changed": changed,
        })
    }
}

/// A request seen by [`mock`].
#[derive(Debug)]
pub struct MockRequest {
    /// `GET`, `POST`, ...
    pub method: String,
    /// Path and query, with the mock's `prefix` removed.
    pub target: String,
    /// Headers, names lowercased.
    pub headers: Vec<(String, String)>,
    /// JSON body, `null` if absent or not JSON.
    pub body: Value,
    /// The body as text (e.g. a form).
    pub text: String,
}

impl MockRequest {
    /// The header `name` (lowercase).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Every `"METHOD target"` a [`mock`] served, in order.
pub type MockLog = Arc<Mutex<Vec<String>>>;

/// For tests: a one-thread HTTP/1.1 server on loopback that answers each
/// request with `handler`'s `(status, JSON)`. Returns the base URL
/// (`http://127.0.0.1:PORT{prefix}`) and the request log.
pub fn mock(
    prefix: &str,
    mut handler: impl FnMut(&MockRequest) -> (u16, Value) + Send + 'static,
) -> io::Result<(String, MockLog)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let url = format!("http://{}{prefix}", listener.local_addr()?);
    let log = MockLog::default();
    let seen = log.clone();
    let prefix = prefix.to_owned();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let Ok(req) = read_request(&stream, &prefix) else {
                continue;
            };
            if let Ok(mut log) = seen.lock() {
                log.push(format!("{} {}", req.method, req.target));
            }
            let (status, reply) = handler(&req);
            let reply = reply.to_string();
            let _ = write!(
                &stream,
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                reply.len()
            );
        }
    });
    Ok((url, log))
}

fn read_request(stream: &TcpStream, prefix: &str) -> io::Result<MockRequest> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or_default();
    let target = target.strip_prefix(prefix).unwrap_or(target).to_owned();
    let mut headers = Vec::new();
    loop {
        line.clear();
        reader.read_line(&mut line)?;
        let Some((k, v)) = line.trim_end().split_once(':') else {
            break;
        };
        headers.push((k.trim().to_ascii_lowercase(), v.trim().to_owned()));
    }
    let mut req = MockRequest {
        method,
        target,
        headers,
        body: Value::Null,
        text: String::new(),
    };
    let len = req
        .header("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0; len];
    reader.read_exact(&mut body)?;
    req.body = serde_json::from_slice(&body).unwrap_or(Value::Null);
    req.text = String::from_utf8_lossy(&body).into_owned();
    Ok(req)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn check_time_takes_rfc3339_only() {
        for good in ["2026-10-07T12:00:00Z", "2026-10-07T12:00:00.123+02:00"] {
            assert_eq!(check_time(good).unwrap(), good);
        }
        for bad in [
            "2026-10-07",
            "2026-10-07T12:00:00Z' or '1'='1",
            "20261007T120000Z00",
        ] {
            assert!(check_time(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn send_maps_statuses_to_codes() {
        let (url, log) = mock("/api", |r| match r.target.as_str() {
            "/ok?x=1" => (200, json!({ "auth": r.header("authorization") })),
            "/busy" => (503, json!({ "message": "later" })),
            _ => (404, json!({ "error": "nope" })),
        })
        .unwrap();
        let agent = agent();
        let auth = [("authorization", "Bearer t")];
        let ok: Value = send(&agent, "GET", &url, "/ok?x=1", &auth, None)
            .unwrap()
            .json(false)
            .unwrap();
        assert_eq!(ok, json!({ "auth": "Bearer t" }));
        let busy = send(&agent, "POST", &url, "/busy", &[], Some(&json!({})));
        let err = busy.unwrap().json::<Value>(false).unwrap_err();
        assert_eq!(err.code, code::UNAVAILABLE);
        assert_eq!(err.message, "POST /busy: HTTP 503 later");
        assert_eq!(err.http_status(), Some(503));
        let missing = send(&agent, "GET", &url, "/x", &[], None).unwrap();
        let missing = missing.json::<Value>(false).unwrap_err();
        assert_eq!(
            (missing.code, missing.http_status()),
            (code::FAILED, Some(404))
        );
        assert_eq!(RpcError::failed("HTTP 401 in the text").http_status(), None);
        assert_eq!(log.lock().unwrap()[0], "GET /ok?x=1");
    }

    #[test]
    fn api_url_must_be_https_or_loopback() {
        assert!(check_api_url("https://slack.com/api/").is_ok_and(|u| u.ends_with("/api")));
        assert!(check_api_url("http://127.0.0.1:9/api").is_ok());
        assert!(check_api_url("http://example.com").is_err());
        assert_eq!(encode("a/b c"), "a%2Fb%20c");
        assert!(idem_marker("k:1").is_ok() && idem_marker("bad key").is_err());
    }

    #[test]
    fn credentials_are_taken_out_of_params() {
        let mut p =
            json!({ "repo": "a/b", "credentials": { "token": "t0k", "api_url": "https://x.io/" } });
        let got = take_credentials(&mut p).unwrap();
        assert_eq!(got, Some(("t0k".into(), "https://x.io".into())));
        assert_eq!(p, json!({ "repo": "a/b" }));
        assert_eq!(take_credentials(&mut p).unwrap(), None);
        for bad in [
            json!({ "credentials": { "token": "t", "api_url": "http://evil.io" } }),
            json!({ "credentials": { "token": "", "api_url": "https://x.io" } }),
            json!({ "credentials": "t" }),
        ] {
            let mut p = bad;
            assert_eq!(
                take_credentials(&mut p).unwrap_err().code,
                code::INVALID_PARAMS
            );
        }
    }

    #[test]
    fn commit_status_is_validated() {
        let ok = json!({ "repo": "a/b", "sha": "abc1234", "state": "pending", "context": "sun/merge-order" });
        let s = CommitStatus::parse(ok.clone()).unwrap();
        assert!(s.matches("pending", None) && !s.matches("pending", Some("x")));
        for (field, bad) in [
            ("sha", json!("xyz")),
            ("state", json!("error")),
            ("context", json!(" ")),
            ("description", json!("d".repeat(141))),
            ("target_url", json!("javascript:x")),
        ] {
            let mut p = ok.clone();
            p[field] = bad;
            assert_eq!(
                CommitStatus::parse(p).unwrap_err().code,
                code::INVALID_PARAMS
            );
        }
    }
}
