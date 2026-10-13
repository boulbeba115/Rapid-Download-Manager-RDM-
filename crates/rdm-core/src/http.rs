//! HTTP client construction and the probe request.

use std::time::Duration;

use reqwest::header::{self, HeaderMap};
use reqwest::{Client, RequestBuilder, Response, StatusCode};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::filename;

/// Deliberately honest. Claiming to be a browser without a browser's TLS and
/// header fingerprint gets connections reset by bot filters (observed in
/// testing); downloads captured from a browser carry that browser's own
/// User-Agent via `RequestSpec::user_agent` instead.
pub const DEFAULT_USER_AGENT: &str = concat!("RapidDownloadManager/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ProxyConfig {
    /// Whatever Windows is configured to use.
    #[default]
    System,
    None,
    /// `http://host:port`, `https://host:port` or `socks5://host:port`.
    Manual {
        url: String,
        username: String,
        /// Encrypted in the settings file.
        #[serde(with = "crate::security::secret")]
        password: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NetConfig {
    pub user_agent: String,
    pub connect_timeout_secs: u64,
    pub proxy: ProxyConfig,
}

impl Default for NetConfig {
    fn default() -> Self {
        Self { user_agent: DEFAULT_USER_AGENT.into(), connect_timeout_secs: 15, proxy: ProxyConfig::System }
    }
}

pub fn build_client(cfg: &NetConfig) -> Result<Client> {
    let mut b = Client::builder()
        .user_agent(cfg.user_agent.clone())
        .connect_timeout(Duration::from_secs(cfg.connect_timeout_secs.max(1)))
        // One TCP connection per segment is the point: HTTP/2 would multiplex
        // every range onto a single connection and a single congestion window.
        .http1_only()
        .tcp_nodelay(true)
        .tcp_keepalive(Duration::from_secs(30))
        .pool_max_idle_per_host(64)
        .pool_idle_timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::limited(10));
    match &cfg.proxy {
        ProxyConfig::System => {}
        ProxyConfig::None => b = b.no_proxy(),
        ProxyConfig::Manual { url, username, password } => {
            let mut proxy = reqwest::Proxy::all(url.as_str()).map_err(|e| Error::Other(format!("bad proxy: {e}")))?;
            if !username.is_empty() {
                proxy = proxy.basic_auth(username, password);
            }
            b = b.proxy(proxy);
        }
    }
    b.build().map_err(Error::from)
}

/// Everything needed to ask the server for the file again.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RequestSpec {
    pub url: String,
    pub referer: Option<String>,
    /// The browser's session for this site. Encrypted in the saved download list.
    #[serde(with = "crate::security::secret_opt")]
    pub cookies: Option<String>,
    pub user_agent: Option<String>,
    /// HTTP Basic credentials. The password is encrypted in the saved download list.
    #[serde(with = "crate::security::secret_pair")]
    pub auth: Option<(String, String)>,
    pub headers: Vec<(String, String)>,
}

impl RequestSpec {
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into(), ..Default::default() }
    }
}

/// A GET for `url` on behalf of `spec`. `url` is not always `spec.url`: it
/// may be where a redirect ended up, or a segment or key named by a playlist.
/// The download's cookies, password and custom headers go along only as far
/// as [`credential_scope`](crate::security::credential_scope) allows, so a
/// redirect or a playlist cannot be used to collect them.
pub fn request(client: &Client, spec: &RequestSpec, url: &str) -> RequestBuilder {
    let (cookies_ok, secrets_ok) = crate::security::credential_scope(&spec.url, url);
    // Ranges address the bytes on disk, so the body must never be compressed in transit.
    let mut rb = client.get(url).header(header::ACCEPT, "*/*").header(header::ACCEPT_ENCODING, "identity");
    if let Some(v) = spec.referer.as_deref().filter(|v| !v.is_empty()) {
        rb = rb.header(header::REFERER, v);
    }
    if let Some(v) = spec.cookies.as_deref().filter(|v| cookies_ok && !v.is_empty()) {
        rb = rb.header(header::COOKIE, v);
    }
    if let Some(v) = spec.user_agent.as_deref().filter(|v| !v.is_empty()) {
        rb = rb.header(header::USER_AGENT, v);
    }
    if secrets_ok {
        for (k, v) in &spec.headers {
            rb = rb.header(k.as_str(), v.as_str());
        }
        if let Some((user, pass)) = &spec.auth {
            rb = rb.basic_auth(user, Some(pass));
        }
    }
    rb
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteInfo {
    /// URL after redirects.
    pub final_url: String,
    pub size: Option<u64>,
    /// Server honours `Range`, so the download can be segmented and resumed.
    pub resumable: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_type: Option<String>,
    pub file_name: String,
}

impl RemoteInfo {
    /// Value for `If-Range`. Weak ETags are not allowed there; fall back to the date.
    pub fn validator(&self) -> Option<String> {
        match &self.etag {
            Some(tag) if !tag.starts_with("W/") => Some(tag.clone()),
            _ => self.last_modified.clone(),
        }
    }
}

/// Result of the first request. The still-open response is handed to the first
/// connection so its body is not thrown away.
pub struct Probe {
    pub info: RemoteInfo,
    pub response: Option<Response>,
}

fn header_str(headers: &HeaderMap, name: header::HeaderName) -> Option<String> {
    headers.get(name).and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// `bytes 0-99/1234` → `(0, 99, Some(1234))`.
pub fn parse_content_range(value: &str) -> Option<(u64, u64, Option<u64>)> {
    let rest = value.trim().strip_prefix("bytes")?.trim_start_matches([' ', '=']);
    let (range, total) = rest.split_once('/')?;
    let (start, end) = range.trim().split_once('-')?;
    let total = total.trim();
    let total = if total == "*" { None } else { Some(total.parse().ok()?) };
    Some((start.trim().parse().ok()?, end.trim().parse().ok()?, total))
}

pub async fn probe(client: &Client, spec: &RequestSpec) -> Result<Probe> {
    if url::Url::parse(&spec.url).map(|u| !matches!(u.scheme(), "http" | "https")).unwrap_or(true) {
        return Err(Error::InvalidUrl(spec.url.clone()));
    }
    let mut resp = request(client, spec, &spec.url).header(header::RANGE, "bytes=0-").send().await?;
    if resp.status() == StatusCode::RANGE_NOT_SATISFIABLE {
        // Empty file, or a server that dislikes open-ended ranges.
        resp = request(client, spec, &spec.url).send().await?;
    }
    let status = resp.status();
    if !status.is_success() {
        return Err(Error::Status(status.as_u16()));
    }

    let headers = resp.headers();
    let (size, resumable) = if status == StatusCode::PARTIAL_CONTENT {
        match header_str(headers, header::CONTENT_RANGE).as_deref().and_then(parse_content_range) {
            Some((0, _, Some(total))) => (Some(total), true),
            _ => (None, false),
        }
    } else {
        let len = header_str(headers, header::CONTENT_LENGTH).and_then(|v| v.parse::<u64>().ok());
        // Some servers answer `bytes=0-` with a plain 200 yet do support ranges.
        // Trust the advertisement; the engine falls back if it proves false.
        let advertised =
            header_str(headers, header::ACCEPT_RANGES).map(|v| v.eq_ignore_ascii_case("bytes")).unwrap_or(false);
        (len, advertised && len.is_some())
    };

    let final_url = resp.url().to_string();
    let content_type = header_str(headers, header::CONTENT_TYPE);
    let file_name = filename::pick(
        header_str(headers, header::CONTENT_DISPOSITION).as_deref(),
        &final_url,
        content_type.as_deref(),
    );
    let info = RemoteInfo {
        final_url,
        size,
        resumable,
        etag: header_str(headers, header::ETAG),
        last_modified: header_str(headers, header::LAST_MODIFIED),
        content_type,
        file_name,
    };
    Ok(Probe { info, response: Some(resp) })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A redirect target or a playlist entry on someone else's server must not
    /// receive the session cookie, the password or the custom headers.
    #[test]
    fn credentials_do_not_follow_a_request_to_another_site() {
        let client = Client::new();
        let spec = RequestSpec {
            url: "https://files.example/get?id=1".into(),
            referer: Some("https://files.example/".into()),
            cookies: Some("sid=secret".into()),
            auth: Some(("user".into(), "password".into())),
            headers: vec![("X-Api-Key".into(), "key".into())],
            ..Default::default()
        };
        let sent = |url: &str| {
            let req = request(&client, &spec, url).build().unwrap();
            let has = |name: &str| req.headers().contains_key(name);
            (has("cookie"), has("authorization"), has("x-api-key"), has("referer"))
        };
        assert_eq!(sent(&spec.url), (true, true, true, true));
        assert_eq!(sent("https://files.example/other"), (true, true, true, true));
        assert_eq!(sent("https://cdn.files.example/part1"), (true, false, false, true));
        assert_eq!(sent("https://collector.evil.test/x"), (false, false, false, true));
        assert_eq!(sent("http://files.example/get?id=1"), (false, false, false, true));
    }

    #[test]
    fn content_range() {
        assert_eq!(parse_content_range("bytes 0-99/1234"), Some((0, 99, Some(1234))));
        assert_eq!(parse_content_range("bytes 5-9/*"), Some((5, 9, None)));
        assert_eq!(parse_content_range("bytes */1234"), None);
        assert_eq!(parse_content_range("items 0-1/2"), None);
    }
}
