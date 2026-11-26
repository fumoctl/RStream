use anyhow::{anyhow, bail, ensure, Context, Result};
use reqwest::{header, Client, StatusCode};
use serde_json::{json, Map, Value};
use std::time::Duration;
use url::{Host, Url};

#[derive(Clone, Debug)]
pub(crate) struct Addon {
    pub url: String,
    /// Shared: the parsed manifest is cached and handed to every request without copying.
    pub manifest: std::sync::Arc<Value>,
    pub allow_http: bool,
    pub generation: String,
}
impl Addon {
    pub fn descriptor(&self) -> Value {
        let mut v = (*self.manifest).clone();
        v["manifest_url"] = json!(self.url);
        v
    }
    pub fn name(&self) -> String {
        self.manifest["name"]
            .as_str()
            .unwrap_or("Unnamed addon")
            .to_owned()
    }
    pub fn supports(&self, resource: &str, kind: &str, id: &str) -> bool {
        let Some(resources) = self.manifest["resources"].as_array() else {
            return false;
        };
        resources.iter().any(|r| {
            let (name, types, prefixes) = if let Some(name) = r.as_str() {
                (
                    Some(name),
                    self.manifest.get("types"),
                    self.manifest.get("idPrefixes"),
                )
            } else {
                (
                    r["name"].as_str(),
                    r.get("types").or_else(|| self.manifest.get("types")),
                    r.get("idPrefixes")
                        .or_else(|| self.manifest.get("idPrefixes")),
                )
            };
            name == Some(resource)
                && includes(types, kind)
                && prefixes
                    .map(|p| {
                        p.as_array().is_some_and(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .any(|prefix| id.starts_with(prefix))
                        })
                    })
                    .unwrap_or(true)
        })
    }
    pub fn catalog(&self, kind: &str, id: &str) -> Option<&Value> {
        self.manifest["catalogs"]
            .as_array()?
            .iter()
            .find(|c| c["type"] == kind && c["id"] == id)
    }
}
fn includes(values: Option<&Value>, needle: &str) -> bool {
    values
        .map(|v| {
            v.as_array()
                .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(needle)))
        })
        .unwrap_or(true)
}
/// Errors cross into UI and logs; configured addon URLs often carry provider
/// tokens in the path or query. Keep only scheme + host so secrets never escape.
pub(crate) fn redact(input: impl AsRef<str>) -> String {
    let input = input.as_ref();
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(pos) = rest.find("http") {
        let candidate = &rest[pos..];
        let scheme_len = if candidate.starts_with("https://") {
            8
        } else if candidate.starts_with("http://") {
            7
        } else {
            0
        };
        if scheme_len == 0 {
            let take = pos + 1;
            out.push_str(&rest[..take]);
            rest = &rest[take..];
            continue;
        }
        let mut end = scheme_len;
        let bytes = candidate.as_bytes();
        while end < bytes.len() && !b" \t\r\n()[]<>\"'".contains(&bytes[end]) {
            end += 1;
        }
        let token = &candidate[..end];
        let trimmed = token.trim_end_matches(['.', ',', ';', ':']);
        let trailing = &token[trimmed.len()..];
        let safe = match Url::parse(trimmed) {
            Ok(u) => {
                let host = u.host_str().unwrap_or("...");
                format!("{}://{}/...", u.scheme(), host)
            }
            Err(_) => "<url>".to_owned(),
        };
        out.push_str(&safe);
        out.push_str(trailing);
        rest = &candidate[end..];
    }
    out.push_str(rest);
    out
}

pub(crate) fn normalize(input: &str, allow_http: bool) -> Result<String> {
    let input = input.trim();
    ensure!(
        !input.is_empty() && input.len() <= 16384,
        "Invalid addon URL"
    );
    let converted;
    let input = if let Some(rest) = input.strip_prefix("stremio://") {
        converted = format!("https://{rest}");
        &converted
    } else {
        input
    };
    let mut url = Url::parse(input).map_err(|_| anyhow!("Invalid addon URL"))?;
    validate_transport(&url, allow_http)?;
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "URL credentials are unsupported; use configured addon paths"
    );
    url.set_fragment(None);
    if !url.path().ends_with("/manifest.json") {
        let path = format!("{}/manifest.json", url.path().trim_end_matches('/'));
        url.set_path(&path);
    }
    Ok(url.to_string())
}
pub(crate) fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => {
            ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
        Some(Host::Domain(d)) => {
            let d = d.trim_end_matches('.');
            d.eq_ignore_ascii_case("localhost") || d.to_ascii_lowercase().ends_with(".localhost")
        }
        None => false,
    }
}
fn is_unspecified(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(ip)) => ip.is_unspecified(),
        Some(Host::Ipv6(ip)) => {
            ip.is_unspecified() || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_unspecified())
        }
        _ => false,
    }
}
pub(crate) fn validate_redirect(started_loopback: bool, next: &Url) -> Result<()> {
    ensure!(
        !is_unspecified(next) && (started_loopback || !is_loopback(next)),
        "Redirect to loopback is not allowed"
    );
    Ok(())
}
pub(crate) fn validate_transport(url: &Url, allow_http: bool) -> Result<()> {
    let loopback = is_loopback(url);
    ensure!(
        url.host_str().is_some() && !is_unspecified(url),
        "Addon URL needs a valid host"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "URL credentials are unsupported"
    );
    ensure!(
        url.scheme() == "https" || (url.scheme() == "http" && (loopback || allow_http)),
        "Non-loopback HTTP requires allow_http: true"
    );
    Ok(())
}
fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            use std::fmt::Write;
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}
pub(crate) fn endpoint(
    manifest: &str,
    resource: &str,
    kind: &str,
    id: &str,
    extra: &Map<String, Value>,
) -> Result<String> {
    ensure!(
        !matches!(kind, "." | "..") && !matches!(id, "." | ".."),
        "Dot path segments are unsupported media identities"
    );
    let mut url = Url::parse(manifest).map_err(|_| anyhow!("Invalid addon URL"))?;
    let base = url
        .path()
        .strip_suffix("/manifest.json")
        .context("Expected manifest.json URL")?;
    let mut path = format!(
        "{base}/{}/{}/{}",
        encode(resource),
        encode(kind),
        encode(id)
    );
    if !extra.is_empty() {
        path.push('/');
        let parts = extra
            .iter()
            .map(|(k, v)| {
                let v = v
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| v.to_string());
                format!("{}={}", encode(k), encode(&v))
            })
            .collect::<Vec<_>>();
        path.push_str(&parts.join("&"));
    }
    path.push_str(".json");
    url.set_path(&path);
    url.set_fragment(None);
    Ok(url.to_string())
}
pub(crate) fn validate_manifest(mut v: Value) -> Result<Value> {
    ensure!(v.is_object(), "Manifest must be an object");
    for key in ["id", "name", "version"] {
        ensure!(
            v.get(key)
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty()),
            "Manifest missing {key}"
        );
    }
    for key in ["resources", "types", "catalogs"] {
        if v.get(key).is_none() {
            v[key] = json!([]);
        }
        ensure!(v[key].is_array(), "Manifest {key} must be an array");
    }
    v.as_object_mut().unwrap().remove("bootstrap");
    Ok(v)
}
pub(crate) fn validate_extras(catalog: &Value, extras: &Map<String, Value>) -> Result<()> {
    if let Some(list) = catalog["extra"].as_array() {
        for e in list {
            if e["isRequired"] == true {
                let name = e["name"]
                    .as_str()
                    .context("Invalid required catalog extra")?;
                ensure!(
                    extras
                        .get(name)
                        .is_some_and(|v| !v.is_null() && v.as_str() != Some("")),
                    "Required catalog extra missing: {name}"
                );
            }
        }
    }
    if let Some(list) = catalog["extraRequired"].as_array() {
        for name in list.iter().filter_map(Value::as_str) {
            ensure!(
                extras
                    .get(name)
                    .is_some_and(|v| !v.is_null() && v.as_str() != Some("")),
                "Required catalog extra missing: {name}"
            );
        }
    }
    Ok(())
}
pub(crate) fn searchable(catalog: &Value) -> bool {
    catalog["extra"]
        .as_array()
        .is_some_and(|a| a.iter().any(|e| e["name"] == "search"))
        || catalog["extraSupported"]
            .as_array()
            .is_some_and(|a| a.iter().any(|e| e == "search"))
        || catalog["extraRequired"]
            .as_array()
            .is_some_and(|a| a.iter().any(|e| e == "search"))
}
pub(crate) struct CachePolicy {
    pub no_store: bool,
    pub private: bool,
    pub ttl: i64,
}
/// Combine repeated `Cache-Control` and `Age` headers conservatively so a restrictive
/// second line can never be hidden by a permissive first one.
pub(crate) fn cache_policy(
    headers: &header::HeaderMap,
    default_ttl: i64,
    max_ttl: i64,
) -> CachePolicy {
    let mut no_store = false;
    let mut private = false;
    let mut force_stale = false;
    let mut max_age: Option<i64> = None;
    for line in headers.get_all(header::CACHE_CONTROL) {
        let Ok(line) = line.to_str() else {
            force_stale = true;
            continue;
        };
        for directive in line.split(',') {
            let (name, value) = directive
                .trim()
                .split_once('=')
                .unwrap_or((directive.trim(), ""));
            let name = name.trim();
            if name.eq_ignore_ascii_case("no-store") {
                no_store = true;
            } else if name.eq_ignore_ascii_case("private") {
                private = true;
            } else if name.eq_ignore_ascii_case("no-cache") {
                force_stale = true;
            } else if name.eq_ignore_ascii_case("max-age") {
                match value.trim().trim_matches('"').parse::<i64>() {
                    Ok(s) if s >= 0 => max_age = Some(max_age.map_or(s, |old| old.min(s))),
                    _ => force_stale = true,
                }
            }
        }
    }
    let mut age = 0i64;
    for v in headers.get_all(header::AGE) {
        match v
            .to_str()
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .filter(|&a| a >= 0)
        {
            Some(a) => age = age.max(a),
            None => force_stale = true,
        }
    }
    let ttl = if force_stale {
        0
    } else {
        max_age
            .unwrap_or(default_ttl)
            .clamp(0, max_ttl)
            .saturating_sub(age)
    };
    CachePolicy {
        no_store,
        private,
        ttl,
    }
}
#[derive(Debug)]
pub(crate) struct Fetched {
    pub body: Value,
    pub ttl: i64,
    pub persist: bool,
}
pub(crate) async fn fetch(
    client: &Client,
    target: &str,
    allow_http: bool,
    default_ttl: i64,
    max_ttl: i64,
) -> Result<Fetched> {
    let mut url = Url::parse(target).map_err(|_| anyhow!("Invalid addon URL"))?;
    let started_loopback = is_loopback(&url);
    for redirects in 0..=3 {
        validate_transport(&url, allow_http)?;
        let mut response = client.get(url.clone()).send().await.map_err(|e| {
            if e.is_timeout() {
                anyhow!("Addon request timed out")
            } else {
                anyhow!("Addon request failed")
            }
        })?;
        if response.status().is_redirection() {
            ensure!(redirects < 3, "Too many addon redirects");
            let location = response
                .headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .context("Invalid addon redirect")?;
            url = url
                .join(location)
                .map_err(|_| anyhow!("Invalid addon redirect"))?;
            validate_redirect(started_loopback, &url)?;
            continue;
        }
        ensure!(
            response.status() == StatusCode::OK,
            "Addon returned HTTP {}",
            response.status().as_u16()
        );
        let policy = cache_policy(response.headers(), default_ttl, max_ttl);
        let persist = !policy.no_store && !policy.private;
        let ttl = policy.ttl;
        ensure!(
            response.content_length().unwrap_or(0) <= 8 * 1024 * 1024,
            "Addon response too large"
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| anyhow!("Could not read addon response"))?
        {
            ensure!(
                bytes.len() + chunk.len() <= 8 * 1024 * 1024,
                "Addon response too large"
            );
            bytes.extend_from_slice(&chunk);
        }
        let body: Value =
            serde_json::from_slice(&bytes).map_err(|_| anyhow!("Addon returned invalid JSON"))?;
        ensure!(body.is_object(), "Addon response must be an object");
        return Ok(Fetched { body, ttl, persist });
    }
    bail!("Too many addon redirects")
}
pub(crate) fn client() -> Result<Client> {
    Ok(Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(12))
        .user_agent("RustMedia/0.1 (Stremio addon protocol)")
        .build()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transport_consent_and_namespaces() {
        assert!(normalize("http://example.com/manifest.json", false).is_err());
        assert!(normalize("http://example.com/manifest.json", true).is_ok());
        assert!(normalize("http://127.0.0.1:1234/manifest.json", false).is_ok());
        assert!(normalize("http://[::1]:1234/manifest.json", false).is_ok());
        assert!(normalize("file:///manifest.json", true).is_err());
        assert_eq!(
            normalize("stremio://example.com/config/manifest.json?key=a", false).unwrap(),
            "https://example.com/config/manifest.json?key=a"
        );
        assert!(normalize("http://0.0.0.0:1234/manifest.json", true).is_err());
        assert!(normalize("https://[::]:1234/manifest.json", true).is_err());
    }

    #[tokio::test]
    async fn fetch_blocks_remote_to_loopback_and_unspecified_redirects() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        for location in [
            "http://127.0.0.1:8080/manifest.json",
            "https://127.0.0.1:8443/manifest.json",
            "http://[::1]:8080/manifest.json",
            "https://localhost/manifest.json",
            "http://sub.localhost./manifest.json",
            "http://0.0.0.0:8080/manifest.json",
            "http://[::ffff:127.0.0.1]:8080/manifest.json",
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let loc = location.to_owned();
            let handle = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let reply = format!(
                    "HTTP/1.1 302 Found\r\nLocation: {loc}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = stream.write_all(reply.as_bytes());
            });
            let client = Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .proxy(reqwest::Proxy::http(format!("http://{addr}")).unwrap())
                .build()
                .unwrap();
            let err = fetch(
                &client,
                "http://remote.addon.example/manifest.json",
                true,
                60,
                60,
            )
            .await
            .unwrap_err();
            handle.join().unwrap();
            assert!(
                err.to_string().contains("Redirect to loopback is not allowed"),
                "location {location} gave unexpected error: {err}"
            );
        }
    }
}

#[cfg(test)]
mod protocol_tests {
    use super::*;
    #[test]
    fn configured_paths_queries_and_resource_overrides() {
        let endpoint = endpoint(
            "https://example.com/config%2Fsecret/manifest.json?x=a%2Bb",
            "meta",
            "anime",
            "kitsu:123/1",
            &Map::new(),
        )
        .unwrap();
        assert_eq!(
            endpoint,
            "https://example.com/config%2Fsecret/meta/anime/kitsu%3A123%2F1.json?x=a%2Bb"
        );
        let a = Addon {
            url: "https://example.com/manifest.json".into(),
            allow_http: false,
            generation: String::new(),
            manifest: json!({
                "types":["series"],"idPrefixes":["tt"],"resources":["subtitles",{"name":"stream","types":["anime"],"idPrefixes":["kitsu:"]}]
            })
            .into(),
        };
        assert!(a.supports("subtitles", "series", "tt1"));
        assert!(!a.supports("subtitles", "anime", "kitsu:1"));
        assert!(a.supports("stream", "anime", "kitsu:1"));
        assert!(!a.supports("stream", "series", "tt1"));
        assert!(super::endpoint(&a.url, "meta", "..", "tt1", &Map::new()).is_err());
    }
}

#[cfg(test)]
mod redaction_tests {
    use super::redact;
    #[test]
    fn strips_path_query_and_credentials() {
        let msg = redact("GET https://user:pass@addon.example/sort=size|apikey=SECRETKEY/manifest.json?api=ALSO_SECRET failed");
        assert!(!msg.contains("SECRET"), "{msg}");
        assert!(!msg.contains("user"), "{msg}");
        assert!(msg.contains("https://addon.example/..."), "{msg}");
    }
    #[test]
    fn keeps_non_url_text_and_punctuation() {
        assert_eq!(redact("plain error, no url."), "plain error, no url.");
    }
}
