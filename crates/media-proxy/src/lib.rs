//! Origin-isolated HTTP(S) media source prober.
use anyhow::{anyhow, ensure, Context, Result};
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue},
    Client, Method, Response,
};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::time::timeout;
use url::Url;

const HEADER_LIMIT: usize = 8192;
const IO_TIMEOUT: Duration = Duration::from_secs(10);
/// Smallest size a probed source may have to count as a film (status clips are smaller).
const PROBE_MIN_BYTES: u64 = 50 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeOutcome {
    pub ok: bool,
    pub status: u16,
    pub bytes: Option<u64>,
    pub provider_status: Option<&'static str>,
    pub playlist: bool,
    pub resolved_url: Option<String>,
}

pub struct MediaProxy {
    client: Client,
}

impl Default for MediaProxy {
    fn default() -> Self {
        Self::new()
    }
}

pub fn validate_headers(value: &Value) -> Result<()> {
    scoped_headers(value).map(|_| ())
}

pub fn validate_source(url: &str, allow_http: bool) -> Result<String> {
    validate_reference(url)?;
    let parsed = Url::parse(url).context("invalid media URL")?;
    let allow_loopback = allow_http && is_loopback_host(&parsed);
    validate_url(&parsed, allow_http, allow_loopback)?;
    Ok(parsed.to_string())
}

impl MediaProxy {
    pub fn new() -> Self {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .http1_only()
            .pool_max_idle_per_host(4)
            .pool_idle_timeout(Duration::from_secs(15))
            .connect_timeout(IO_TIMEOUT)
            .build()
            .expect("rustls client initialization");
        Self { client }
    }

    /// Checks that a source really delivers a film before playback: one byte
    /// is requested with origin-scoped headers and redirect/SSRF guards.
    /// Provider status clips, error statuses and tiny files fail.
    pub async fn probe(&self, url: &str, headers: &Value, allow_http: bool) -> Result<Value> {
        validate_reference(url)?;
        let original = Url::parse(url).context("invalid media URL")?;
        let allow_loopback = allow_http && is_loopback_host(&original);
        validate_url(&original, allow_http, allow_loopback)?;
        let configured = scoped_headers(headers)?;
        let (response, final_url) = self
            .fetch_probe(&original, &configured, allow_http, allow_loopback)
            .await?;

        let status = response.status().as_u16();
        if let Some(provider_status) = provider_status_clip(&final_url) {
            return Ok(json!({
                "ok": false,
                "status": status,
                "bytes": null,
                "provider_status": provider_status
            }));
        }

        let resp_headers = response.headers();
        let bytes = match status {
            206 => content_range_total(resp_headers),
            200 => resp_headers
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok()),
            _ => None,
        };
        let content_type = resp_headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let path = final_url.path().to_ascii_lowercase();
        let playlist = content_type.contains("mpegurl")
            || content_type.contains("dash+xml")
            || path.ends_with(".m3u8")
            || path.ends_with(".mpd");
        let ok =
            matches!(status, 200 | 206) && (playlist || bytes.is_none_or(|n| n >= PROBE_MIN_BYTES));
        let resolved_url = (ok && final_url.scheme() == "https").then(|| final_url.as_str());
        Ok(json!({
            "ok": ok,
            "status": status,
            "bytes": bytes,
            "provider_status": null,
            "playlist": playlist,
            "resolved_url": resolved_url
        }))
    }

    async fn fetch_probe(
        &self,
        original: &Url,
        configured: &HeaderMap,
        allow_http: bool,
        allow_loopback: bool,
    ) -> Result<(Response, Url)> {
        let mut url = original.clone();
        for hop in 0..=8 {
            validate_url(&url, allow_http, allow_loopback)?;
            let mut headers = if same_origin(original, &url) {
                configured.clone()
            } else {
                HeaderMap::new()
            };
            headers.insert("accept-encoding", HeaderValue::from_static("identity"));
            headers.insert("range", HeaderValue::from_static("bytes=0-0"));

            let req = self.client.request(Method::GET, url.clone()).headers(headers);
            let response = timeout(IO_TIMEOUT, req.send())
                .await
                .context("upstream response timeout")??;
            ensure!(
                safe_response_headers(response.headers()),
                "unsafe/oversized upstream headers"
            );
            if matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
                ensure!(hop < 8, "redirect limit exceeded");
                let location = response
                    .headers()
                    .get("location")
                    .ok_or_else(|| anyhow!("redirect without Location"))?
                    .to_str()?;
                url = resolve_redirect(&url, location, allow_http, allow_loopback)?;
                continue;
            }
            return Ok((response, url));
        }
        unreachable!()
    }
}

fn resolve_redirect(
    current: &Url,
    location: &str,
    allow_http: bool,
    allow_loopback: bool,
) -> Result<Url> {
    validate_reference(location)?;
    let next = current.join(location)?;
    validate_url(&next, allow_http, allow_loopback)?;
    Ok(next)
}

fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host() == b.host()
        && a.port_or_known_default() == b.port_or_known_default()
}

fn is_unspecified_host(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(v4)) => v4.is_unspecified(),
        Some(url::Host::Ipv6(v6)) => {
            v6.is_unspecified()
                || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_unspecified())
                || v6.to_ipv4().is_some_and(|v4| v4.is_unspecified())
        }
        _ => false,
    }
}

fn is_loopback_host(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(v4)) => v4.is_loopback(),
        Some(url::Host::Ipv6(v6)) => {
            v6.is_loopback()
                || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
                || (!v6.is_unspecified() && v6.to_ipv4().is_some_and(|v4| v4.is_loopback()))
        }
        Some(url::Host::Domain(name)) => {
            let trimmed = name.trim_end_matches('.');
            trimmed.eq_ignore_ascii_case("localhost")
                || trimmed
                    .to_ascii_lowercase()
                    .ends_with(".localhost")
        }
        None => false,
    }
}

fn validate_reference(reference: &str) -> Result<()> {
    ensure!(
        !reference.bytes().any(|b| b <= 32 || b == 127 || b == b'\\'),
        "URL whitespace/control/backslash is forbidden"
    );
    ensure!(!reference.contains('#'), "URL fragments are unsupported");
    let authority = if let Some(rest) = reference.strip_prefix("//") {
        Some(rest)
    } else if let Some((scheme, rest)) = reference.split_once(':') {
        if !scheme.contains(['/', '?']) {
            ensure!(
                scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"),
                "unsupported URL scheme"
            );
            Some(
                rest.strip_prefix("//")
                    .ok_or_else(|| anyhow!("URL authority required"))?,
            )
        } else {
            None
        }
    } else {
        None
    };
    if let Some(authority) = authority {
        let authority = authority.split(['/', '?']).next().unwrap_or("");
        ensure!(
            !authority.is_empty() && !authority.contains('@'),
            "invalid URL authority/userinfo"
        );
    }
    Ok(())
}

fn validate_url(url: &Url, allow_http: bool, allow_loopback: bool) -> Result<()> {
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "URL credentials are forbidden"
    );
    ensure!(url.host().is_some(), "URL must have a host");
    ensure!(
        !is_unspecified_host(url),
        "unspecified host address is forbidden"
    );
    ensure!(
        allow_loopback || !is_loopback_host(url),
        "loopback host is forbidden"
    );
    ensure!(
        url.scheme() == "https" || (url.scheme() == "http" && allow_http),
        "HTTP requires explicit consent; unsupported URL scheme"
    );
    Ok(())
}

fn scoped_headers(value: &Value) -> Result<HeaderMap> {
    if value.is_null() {
        return Ok(HeaderMap::new());
    }
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("headers must be an object"))?;
    let mut out = HeaderMap::new();
    let mut total = 0;
    for (key, value) in object {
        let value = value
            .as_str()
            .ok_or_else(|| anyhow!("header values must be strings"))?;
        ensure!(
            !key.bytes().chain(value.bytes()).any(|b| b < 32 || b == 127),
            "header control character"
        );
        let name = HeaderName::from_bytes(key.as_bytes())?;
        ensure!(
            !matches!(
                name.as_str(),
                "host"
                    | "connection"
                    | "proxy-connection"
                    | "proxy-authorization"
                    | "transfer-encoding"
                    | "content-length"
                    | "te"
                    | "trailer"
                    | "upgrade"
                    | "accept-encoding"
            ),
            "reserved relay header"
        );
        total += key.len() + value.len() + 4;
        ensure!(total <= HEADER_LIMIT, "upstream headers too large");
        ensure!(!out.contains_key(&name), "duplicate configured header");
        out.insert(name, HeaderValue::from_str(value)?);
    }
    Ok(out)
}

fn safe_response_headers(headers: &HeaderMap) -> bool {
    headers
        .iter()
        .map(|(k, v)| k.as_str().len() + v.as_bytes().len() + 4)
        .sum::<usize>()
        <= HEADER_LIMIT
        && !headers
            .values()
            .any(|v| v.as_bytes().iter().any(|b| *b < 32 || *b == 127))
        && [
            "content-type",
            "content-encoding",
            "content-length",
            "content-range",
            "location",
        ]
        .iter()
        .all(|name| headers.get_all(*name).iter().count() <= 1)
}

fn provider_status_clip(url: &Url) -> Option<&'static str> {
    let path = url.path().to_ascii_lowercase();
    let (folder, file) = path.rsplit_once('/')?;
    let stem = file.strip_suffix(".mp4")?;
    if !(folder.ends_with("/videos") || folder.contains("/static") || folder.contains("exceptions"))
    {
        return None;
    }
    if stem.contains("download") || stem.contains("queue") || stem.contains("caching") {
        return Some("downloading");
    }
    const FAILED: [&str; 10] = [
        "fail",
        "error",
        "limit",
        "expired",
        "cancel",
        "infring",
        "unavailable",
        "not_found",
        "too_big",
        "no_matching",
    ];
    FAILED
        .iter()
        .any(|word| stem.contains(word))
        .then_some("failed")
}

fn content_range_total(headers: &HeaderMap) -> Option<u64> {
    headers
        .get("content-range")?
        .to_str()
        .ok()?
        .rsplit_once('/')?
        .1
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssrf_loopback_and_unspecified_guards() {
        for forbidden in [
            "http://127.0.0.1:8080/a",
            "https://127.0.0.1/a",
            "http://127.255.255.254/a",
            "http://[::1]/a",
            "https://[::1]/a",
            "http://[::ffff:127.0.0.1]/a",
            "http://[::ffff:127.1.2.3]/a",
            "http://localhost/a",
            "https://localhost./a",
            "https://sub.localhost/a",
            "http://0.0.0.0/a",
            "https://0.0.0.0/a",
            "http://[::]/a",
            "https://[::]/a",
            "http://[::ffff:0.0.0.0]/a",
        ] {
            assert!(
                validate_source(forbidden, false).is_err(),
                "!allow_http must forbid {forbidden}"
            );
        }

        // Unspecified hosts remain forbidden even when allow_http = true.
        for unspecified in [
            "http://0.0.0.0/a",
            "http://[::]/a",
            "http://[::ffff:0.0.0.0]/a",
        ] {
            assert!(
                validate_source(unspecified, true).is_err(),
                "unspecified must always be forbidden: {unspecified}"
            );
        }

        // Non-loopback origin redirecting to ANY loopback/unspecified host is rejected even with allow_http = true.
        let remote = Url::parse("https://cdn.example/stream.mkv").unwrap();
        for target in [
            "http://127.0.0.1:5572/secret",
            "https://127.0.0.1/secret",
            "http://127.0.0.99/secret",
            "http://[::1]:8080/secret",
            "http://[::ffff:127.0.0.1]:8080/secret",
            "http://localhost:5572/secret",
            "http://0.0.0.0:8080/secret",
            "http://[::]:8080/secret",
        ] {
            assert!(
                resolve_redirect(&remote, target, true, false).is_err(),
                "remote redirect to {target} must be rejected"
            );
        }

        // Direct loopback probing works only when allow_http = true AND initial URL was loopback.
        assert!(validate_source("http://127.0.0.1:9000/movie.mkv", true).is_ok());
    }

    #[test]
    fn redirect_downgrade_and_origin_policy() {
        let original = Url::parse("https://media.example:443/a").unwrap();
        let downgraded = original.join("http://media.example/b").unwrap();
        assert!(validate_url(&downgraded, false, false).is_err());
        assert!(validate_url(&downgraded, true, false).is_ok());
        assert!(!same_origin(&original, &downgraded));
        assert!(same_origin(
            &original,
            &Url::parse("https://media.example/b").unwrap()
        ));
        assert!(!same_origin(
            &original,
            &Url::parse("https://media.example:444/b").unwrap()
        ));
        for reference in [
            "https://@media.example/a",
            "//user@media.example/a",
            "https://media.example/\nfoo",
            "https://media.example/\\x",
            "https://media.example/a#frag",
        ] {
            assert!(validate_reference(reference).is_err(), "{reference}");
        }
    }

    #[test]
    fn provider_status_clips_are_recognised() {
        let status = |url: &str| provider_status_clip(&Url::parse(url).unwrap());
        assert_eq!(
            status("https://streams.example/videos/downloading_v2.mp4"),
            Some("downloading")
        );
        assert_eq!(
            status("https://streams.example/videos/failed_access_v2.mp4"),
            Some("failed")
        );
        assert_eq!(
            status("https://provider.example/static/exceptions/stream_cancelled.mp4"),
            Some("failed")
        );
        assert_eq!(status("https://cdn.example/dl/Film.2160p.REMUX.mkv"), None);
    }
}
