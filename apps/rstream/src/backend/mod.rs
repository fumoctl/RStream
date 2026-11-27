//! The bridge between the Dioxus shell and `rustmedia-core`.
//!
//! Core requests run on one dedicated Tokio runtime; the UI awaits their join
//! handles, so it never needs to know which executor Dioxus itself uses.
//! Nothing here logs addon URLs, request bodies or credentials.
//!
//! This file holds core access, catalogues, the title page, Picks, Search and viewing history;
//! `sources` (Play's choice), `subtitles` (online subtitles, skip times), `metadata` (posters,
//! BetterPosters, TMDB, TVmaze) and `store` (settings, tonight's duel, search history) hold the
//! rest, all re-exported here as `crate::backend::*`.

use anyhow::{anyhow, Context, Result};
use rustmedia_core::Core;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::Duration,
};
use tokio::runtime::Runtime;
use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

fn re(pattern: &str) -> regex::Regex { regex::Regex::new(pattern).expect("static pattern") }
macro_rules! static_re { ($name:ident, $pat:expr) => {
    pub(super) fn $name() -> &'static regex::Regex { static R: OnceLock<regex::Regex> = OnceLock::new(); R.get_or_init(|| re($pat)) }
}; }

mod downloads;
mod metadata;
mod open;
pub(crate) mod remote_db;
mod sources;
mod store;
mod subtitles;
pub use downloads::*;
pub use metadata::*;
pub use open::*;
pub use remote_db::{
    list_downloads_paged, load_binge_lock, profile_sync_epoch, purge_disconnected_remote,
    save_binge_lock, sync_profile_now, trigger_profile_sync, trigger_profile_sync_now,
};
pub use sources::*;
pub use store::*;
pub use subtitles::*;

const CINEMETA: &str = "https://v3-cinemeta.strem.io/manifest.json";
/// Kitsu stays installed only so anime already in your Library keeps opening (its titles have
/// Kitsu ids); it is no longer shown or browsed: Home, Search, Picks and Settings skip it.
const KITSU_HOST: &str = "anime-kitsu.strem.fun";
pub fn retired_addon(manifest_url: &str) -> bool { manifest_url.contains(KITSU_HOST) }
const HISTORY_LIMIT: usize = 12;

struct Backend {
    core: Core,
    proxy: rustmedia_media_proxy::MediaProxy,
    torrent: stremio_torrent::TorrentEngine,
    download_torrent: stremio_torrent::TorrentEngine,
    data_dir: PathBuf,
    http: reqwest::Client,
    accents: Mutex<HashMap<String, Option<String>>>,
}

static RUNTIME: OnceLock<Runtime> = OnceLock::new();
static BACKEND: OnceLock<Backend> = OnceLock::new();

/// The app's own store (library, settings, caches). Override with RSTREAM_DATA.
#[cfg(target_os = "android")]
fn data_dir() -> PathBuf {
    crate::android::files_dir().unwrap_or_else(|_| PathBuf::from("/data/local/tmp/rstream"))
}

#[cfg(not(target_os = "android"))]
fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("RSTREAM_DATA") {
        return PathBuf::from(dir);
    }
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA").or_else(|| std::env::var_os("APPDATA"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
        .unwrap_or_else(|| PathBuf::from("."));
    #[cfg(not(any(windows, target_os = "macos")))]
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("rstream")
}

pub fn init() -> Result<()> {
    // Android can launch the UI again in the same process (activity recreated): keep the backend.
    if BACKEND.get().is_some() { return Ok(()); }
    #[cfg(target_os = "android")]
    crate::android::tls_verifier_init();
    let workers = std::thread::available_parallelism().map(|n| n.get().clamp(2, 4)).unwrap_or(2);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()?;
    let dir = data_dir();
    std::fs::create_dir_all(&dir).context("Couldn't create the data folder")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    // Some Android builds (Huawei) hide app logs: keep the last panic in the app's own folder,
    // scrubbed of URLs/tokens and restricted to owner-only permissions.
    let panic_file = dir.join("last-panic.txt");
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let raw = format!("{info}\n{}", std::backtrace::Backtrace::force_capture());
        let clean = store::redact_diagnostic(&raw);
        let _ = store::write_private_file(&panic_file, clean.as_bytes());
        default_hook(info);
    }));
    let core = Core::open(&dir)?;
    let http = reqwest::Client::builder()
        .user_agent(concat!("RStream/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(8))
        .build()?;
    let proxy = { let _inside = runtime.enter(); rustmedia_media_proxy::MediaProxy::new() };
    let torrent = stremio_torrent::TorrentEngine::new(dir.join("torrent-cache"));
    let download_torrent = stremio_torrent::TorrentEngine::new(dir.join("torrent-downloads"));
    // The settings the backend keeps in memory, read once.
    let saved: Value = std::fs::read_to_string(dir.join("settings.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let killswitch = saved["torrent_killswitch"].as_bool().unwrap_or(false);
    let bind_iface = saved["torrent_bind_interface"].as_str().map(str::trim).filter(|k| !k.is_empty()).map(str::to_owned);
    let vpn_policy = stremio_torrent::VpnPolicy { killswitch, bind_interface: bind_iface };
    torrent.set_vpn_policy(vpn_policy.clone());
    download_torrent.set_vpn_policy(vpn_policy);
    downloads::set_parallel_downloads_limit(saved["max_parallel_downloads"].as_u64().unwrap_or(3).clamp(1, 6) as usize);
    #[cfg(target_os = "android")]
    rstream_rclone::set_open_url_handler(Some(crate::android::open_url));
    let _ = rstream_rclone::init(&dir.join("rclone.conf"));
    BETTER_POSTERS.store(saved["better_posters"].as_bool().unwrap_or(true), std::sync::atomic::Ordering::Relaxed);
    HDR_OUTPUT.store(saved["hdr_output"].as_bool().unwrap_or(true), std::sync::atomic::Ordering::Relaxed);
    set_stream_language(Some(audio_language(&saved)));
    set_tmdb_key(saved["tmdb_key"].as_str().map(str::trim).filter(|k| !k.is_empty()).map(str::to_owned));
    RUNTIME.set(runtime).map_err(|_| anyhow!("Backend already started"))?;
    BACKEND
        .set(Backend { core, proxy, torrent, download_torrent, data_dir: dir, http, accents: Mutex::new(HashMap::new()) })
        .map_err(|_| anyhow!("Backend already started"))?;
    downloads::load_persisted_downloads_queue();
    remote_db::trigger_startup_remote_sync();
    #[cfg(target_os = "android")]
    {
        let _ = crate::android::vpn_init();
        let _ = crate::android::transfer_service_init();
    }
    Ok(())
}

/// Read `close_to_tray` before `App` mounts so desktop window creation uses the saved preference.
#[cfg(not(target_os = "android"))]
pub fn initial_close_to_tray() -> bool {
    let v: Value = std::fs::read_to_string(data_dir().join("settings.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    v["close_to_tray"].as_bool().unwrap_or(true)
}

/// Insert into an in-memory cache of answers that can be fetched again, keeping it bounded: past
/// `cap` entries it starts over (cheaper than tracking use; the big ones fill up slowly).
pub(super) fn cache_insert<K: Eq + std::hash::Hash, V>(map: &mut HashMap<K, V>, cap: usize, key: K, value: V) {
    if map.len() >= cap && !map.contains_key(&key) { map.clear(); }
    map.insert(key, value);
}

pub(crate) fn backend_data_dir() -> PathBuf { backend().data_dir.clone() }
pub fn mpv_config_dir() -> PathBuf { backend().data_dir.join("mpv") }
pub(crate) fn backend_http() -> reqwest::Client { backend().http.clone() }

pub(super) fn apply_torrent_vpn_policy(cfg: &Settings) {
    if let Some(b) = BACKEND.get() {
        let policy = stremio_torrent::VpnPolicy {
            killswitch: cfg.torrent_killswitch,
            bind_interface: cfg.torrent_bind_interface.clone(),
        };
        b.torrent.set_vpn_policy(policy.clone());
        b.download_torrent.set_vpn_policy(policy);
    }
}

pub fn torrent_interfaces() -> Vec<stremio_torrent::NetworkInterface> {
    stremio_torrent::list_interfaces()
}

pub fn android_vpn_state() -> (bool, Option<String>) {
    stremio_torrent::android_vpn_state()
}

/// Prepare a Stremio torrent source for streaming on the loopback HTTP server.
/// Enforces the active VPN killswitch and interface binding policy.
pub async fn prepare_torrent(
    info_hash: &str,
    file_idx: Option<u32>,
    tracker_sources: &[String],
    episode_hint: Option<&str>,
) -> Result<(String, Option<String>)> {
    let mut payload = json!({ "infoHash": info_hash });
    if let Some(idx) = file_idx { payload["fileIdx"] = json!(idx); }
    if !tracker_sources.is_empty() { payload["sources"] = json!(tracker_sources); }
    if let Some(hint) = episode_hint.map(str::trim).filter(|h| !h.is_empty()) {
        payload["episodeHint"] = json!(hint);
    }
    let res = on_runtime(async move { backend().torrent.prepare(payload).await }).await??;
    let url = res["url"].as_str().map(str::to_owned).context("Torrent stream did not return a URL")?;
    let lease_id = res["lease_id"].as_str().filter(|s| !s.is_empty()).map(str::to_owned);
    Ok((url, lease_id))
}

/// Abort any in-flight torrent preparation when the user backs out before playback starts.
pub fn stop_torrent() {
    if let Some(rt) = RUNTIME.get() {
        if let Some(b) = BACKEND.get() {
            rt.spawn(async move { let _ = b.torrent.stop().await; });
        }
    }
}

/// Release a specific torrent playback lease without cancelling replacement preparations.
pub fn stop_torrent_lease(lease_id: String) {
    if let Some(rt) = RUNTIME.get() {
        if let Some(b) = BACKEND.get() {
            rt.spawn(async move { let _ = b.torrent.stop_lease(&lease_id).await; });
        }
    }
}

/// Release a specific torrent playback lease and wait for shutdown before starting the next source.
pub async fn stop_torrent_lease_wait(lease_id: String) {
    if BACKEND.get().is_some() {
        let _ = on_runtime(async move { backend().torrent.stop_lease(&lease_id).await }).await;
    }
}

pub fn start_leak_probe(magnet_uri: &str) -> Result<()> {
    let _guard = RUNTIME.get().map(|rt| rt.enter());
    backend().torrent.start_leak_probe(magnet_uri)
}

pub fn stop_leak_probe() {
    if let Some(b) = BACKEND.get() {
        b.torrent.stop_leak_probe();
    }
}

pub fn leak_probe_status() -> stremio_torrent::LeakProbeStatus {
    BACKEND.get().map(|b| b.torrent.leak_probe_status()).unwrap_or_default()
}

pub fn filter_visible_rclone_remotes(remotes: Vec<rstream_rclone::RemoteInfo>) -> Vec<rstream_rclone::RemoteInfo> {
    let crypt_bases: HashSet<String> = remotes.iter()
        .filter(|r| r.kind == "crypt")
        .map(|r| r.name.clone())
        .collect();
    remotes.into_iter()
        .filter(|r| r.name.strip_suffix("-raw").is_none_or(|base| !crypt_bases.contains(base)))
        .collect()
}

pub async fn list_rclone_remotes() -> Result<Vec<rstream_rclone::RemoteInfo>> {
    let raw = on_runtime(async move {
        tokio::task::spawn_blocking(rstream_rclone::list_remotes).await.context("rclone worker failed")?
    }).await??;
    Ok(filter_visible_rclone_remotes(raw))
}

pub async fn list_rclone_providers() -> Result<Vec<rstream_rclone::ProviderInfo>> {
    on_runtime(async move {
        tokio::task::spawn_blocking(rstream_rclone::list_providers).await.context("rclone worker failed")?
    }).await?
}

pub async fn setup_rclone_remote(req: rstream_rclone::RemoteSetupRequest) -> Result<String> {
    let res = on_runtime(async move {
        tokio::task::spawn_blocking(move || {
            let created = rstream_rclone::setup_remote(req)?;
            let _ = remote_db::sync_remote_db(&created);
            Ok::<String, anyhow::Error>(created)
        }).await.context("rclone worker failed")?
    }).await?;
    #[cfg(target_os = "android")]
    crate::android::finish_oauth(res.is_ok());
    res
}

pub async fn sync_rclone_databases() -> Result<usize> {
    on_runtime(async move {
        tokio::task::spawn_blocking(remote_db::sync_all_remotes_blocking).await.context("rclone sync worker failed")?
    }).await?
}

pub fn cancel_rclone_oauth() {
    std::thread::spawn(rstream_rclone::cancel_oauth);
}

fn backend() -> &'static Backend {
    BACKEND.get().expect("backend::init runs before launch")
}

/// Run a future on the core runtime and await it from any executor.
pub(crate) async fn on_runtime<T: Send + 'static>(f: impl Future<Output = T> + Send + 'static) -> Result<T> {
    let runtime = RUNTIME.get().context("Backend not started")?;
    runtime.spawn(f).await.map_err(|e| anyhow!("Background task failed: {e}"))
}

pub(crate) async fn exec(request: Value) -> Result<Value> {
    let clear_meta = matches!(
        request.get("op").and_then(Value::as_str),
        Some("install_addon" | "remove_addon" | "refresh_addons")
    );
    let res = on_runtime(async move { backend().core.execute(request).await }).await?;
    if clear_meta && res.is_ok() {
        if let Some(Ok(mut c)) = TITLE_META_CACHE.get().map(|m| m.lock()) {
            c.clear();
        }
    }
    res
}

pub async fn sleep(ms: u64) {
    let _ = on_runtime(tokio::time::sleep(Duration::from_millis(ms))).await;
}

#[derive(Clone, Debug, PartialEq)]
pub struct Media {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub addon_url: String,
    pub poster: String,
    pub background: String,
    pub description: String,
    pub year: String,
    pub genres: Vec<String>,
    pub reason: String,
    pub liked: i64,
    pub watched: bool,
    /// Saved to the Library by hand.
    pub saved: bool,
    pub video_watched: bool,
    pub position_ms: i64,
    pub duration_ms: i64,
    pub last_video_id: Option<String>,
    pub raw: Value,
}

pub(crate) fn s(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or_default().trim().to_owned()
}

impl Media {
    pub fn from_json(v: &Value, origin: &str) -> Option<Self> {
        let id = Some(s(v, "id")).filter(|x| !x.is_empty()).or_else(|| Some(s(v, "_id")).filter(|x| !x.is_empty()))?;
        let addon_url = [s(v, "addon_url"), s(v, "_addon_url"), origin.to_owned()]
            .into_iter()
            .find(|x| !x.is_empty())?;
        Some(Media {
            id,
            kind: Some(s(v, "type")).filter(|x| !x.is_empty()).unwrap_or_else(|| "movie".into()),
            name: Some(s(v, "name")).filter(|x| !x.is_empty()).unwrap_or_else(|| "Untitled".into()),
            addon_url,
            poster: s(v, "poster"),
            background: s(v, "background"),
            description: s(v, "description"),
            year: s(v, "releaseInfo"),
            genres: v
                .get("genres")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                .unwrap_or_default(),
            reason: s(v, "reason"),
            liked: v.get("liked").and_then(Value::as_i64).unwrap_or(0),
            watched: v.get("watched").and_then(Value::as_bool).unwrap_or(false),
            saved: ["saved", "library"].iter().any(|k| v.get(*k).and_then(Value::as_bool).unwrap_or(false)),
            video_watched: v.get("video_watched").and_then(Value::as_bool).unwrap_or(false),
            position_ms: v.get("position_ms").and_then(Value::as_i64).unwrap_or(0),
            duration_ms: v.get("duration_ms").and_then(Value::as_i64).unwrap_or(0),
            last_video_id: Some(s(v, "video_id")).filter(|x| !x.is_empty()),
            raw: v.clone(),
        })
    }
    pub fn key(&self) -> String {
        format!("{}|{}|{}", self.kind, self.id, self.addon_url)
    }
    /// Wide artwork when the add-on supplies it, else the poster.
    pub fn backdrop(&self) -> &str {
        if self.background.is_empty() { &self.poster } else { &self.background }
    }
    /// First four-digit year in the release text; ranges like "2010–2017" give 2010.
    pub fn first_year(&self) -> Option<i32> {
        let bytes = self.year.as_bytes();
        (0..bytes.len().saturating_sub(3)).find_map(|i| {
            let slice = &self.year.get(i..i + 4)?;
            let boundary_before = i == 0 || !bytes[i - 1].is_ascii_digit();
            let boundary_after = bytes.get(i + 4).is_none_or(|b| !b.is_ascii_digit());
            (boundary_before && boundary_after && slice.bytes().all(|b| b.is_ascii_digit()))
                .then(|| slice.parse().ok())
                .flatten()
                .filter(|y| (1800..2200).contains(y))
        })
    }
    /// "Movie, 2024" — commas, matching the Android app's meta lines.
    pub fn meta_line(&self, with_type: bool) -> String {
        let mut parts = Vec::new();
        if with_type { parts.push(type_label(&self.kind)); }
        if !self.year.is_empty() { parts.push(self.year.clone()); }
        parts.join(", ")
    }
}

pub fn type_label(kind: &str) -> String {
    match kind {
        "movie" => "Movie".into(),
        other => {
            let mut c = other.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
        }
    }
}

pub fn group_label(kind: &str) -> String {
    match kind {
        "movie" => "Movies".into(),
        "series" => "Series".into(),
        "anime" => "Anime".into(),
        other => type_label(other),
    }
}

pub fn canonical_genre(g: &str) -> String {
    let one = |g: &str| -> String {
        match g.trim().to_lowercase().as_str() {
            "science fiction" | "science-fiction" | "sci-fi" | "scifi" | "sci fi" => "Sci-Fi".into(),
            "children" | "kids" => "Kids".into(),
            "reality-tv" | "reality" => "Reality".into(),
            "talk-show" | "talk" => "Talk".into(),
            "game-show" => "Game Show".into(),
            "tv movie" => "TV Movie".into(),
            "musical" => "Musical".into(),
            "war & politics" => "War & Politics".into(),
            other => {
                let mut c = other.chars();
                c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
            }
        }
    };
    if g.contains(" & ") && !g.eq_ignore_ascii_case("war & politics") {
        g.split(" & ").map(one).collect::<Vec<_>>().join(" & ")
    } else {
        one(g)
    }
}

pub fn tidy(mut m: Media) -> Media {
    let mut seen = HashSet::new();
    m.genres = m.genres.iter().map(|g| canonical_genre(g)).filter(|g| !g.is_empty() && seen.insert(g.clone())).collect();
    m
}

pub fn series_complete(list: &[Episode], seen: &HashSet<String>, now: i64) -> bool {
    let (y, mo, d, _) = civil(now);
    let today = format!("{y:04}-{mo:02}-{d:02}");
    let available: Vec<_> = list.iter().filter(|e| e.season > 0 && e.number > 0
        && e.released.get(..10).is_none_or(|date| date <= today.as_str())).collect();
    !available.is_empty() && available.iter().all(|e| seen.contains(&e.id))
}

// ---------------------------------------------------------------- catalogs

fn metas(v: &Value, origin: &str) -> Vec<Media> {
    v.get("metas")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|m| Media::from_json(m, origin).map(remote_db::canonicalize_catalog_media))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------- home

/// One Home shelf: an add-on catalog, optionally narrowed by one extra (genre).
#[derive(Clone, Debug, PartialEq)]
pub struct CatalogRow {
    pub key: String,
    pub title: String,
    /// Shown under the title only where it tells two shelves apart.
    pub subtitle: Option<String>,
    pub addon_url: String,
    pub kind: String,
    pub id: String,
    pub extra: Option<(String, String)>,
    /// A Top 10 shelf: ten titles (BetterPosters' posters carry each one's place).
    pub ranked: bool,
}

/// Home's "New episodes" shelf (TVmaze's schedules), not an add-on catalog.
const NEW_EPISODES_ROW: &str = "tvmaze|new-episodes";

struct RawRow { key: String, title: String, addon_id: String, addon_name: String, addon_url: String, kind: String, id: String, extra: Option<(String, String)> }

/// A catalog that can't be listed without an extra (search-only and the like): the current
/// `extra[].isRequired`, or the legacy `extraRequired` list.
fn catalog_needs_extra(c: &Value) -> bool {
    c["extra"].as_array().into_iter().flatten().any(|e| e["isRequired"] == true)
        || c["extraRequired"].as_array().is_some_and(|r| !r.is_empty())
}

fn options_contain(extra: &Value, name: &str, option: &str) -> bool {
    extra["name"] == name && extra["options"].as_array().is_some_and(|o| o.iter().any(|x| x == option))
}

/// Same rules as homeCatalogRows + homeShelves in the Android app: three catalogs per
/// add-on without required extras, Cinemeta's animation shelf, and titles that
/// disambiguate repeated catalog names.
pub async fn home_rows() -> Result<Vec<CatalogRow>> {
    // Manifests are refreshed once per launch (addons::startup), not on every visit to Home.
    let v = exec(json!({"op":"addons"})).await?;
    let mut raw = Vec::new();
    for addon in v["addons"].as_array().into_iter().flatten() {
        let url = s(addon, "manifest_url");
        if retired_addon(&url) { continue; }
        let (aid, aname) = (s(addon, "id"), s(addon, "name"));
        let catalogs: Vec<&Value> = addon["catalogs"].as_array().into_iter().flatten().collect();
        let free = catalogs.iter().filter(|c| !catalog_needs_extra(c));
        for c in free.take(3) {
            let (kind, id) = (s(c, "type"), s(c, "id"));
            let name = Some(s(c, "name")).filter(|n| !n.is_empty()).unwrap_or_else(|| kind.clone());
            raw.push(RawRow { key: format!("{url}|{kind}|{id}"), title: name, addon_id: aid.clone(), addon_name: aname.clone(),
                addon_url: url.clone(), kind, id, extra: None });
        }
        let animation = catalogs.iter().find(|c| aid == "com.linvo.cinemeta" && c["type"] == "series" && c["id"] == "top"
            && c["extra"].as_array().into_iter().flatten().any(|e| options_contain(e, "genre", "Animation")));
        if let Some(c) = animation {
            raw.push(RawRow { key: format!("{url}|series|top|genre=Animation"), title: "Anime & animation".into(), addon_id: aid.clone(),
                addon_name: aname.clone(), addon_url: url.clone(), kind: s(c, "type"), id: s(c, "id"), extra: Some(("genre".into(), "Animation".into())) });
        }
    }
    // BetterPosters on: its charts lead Home in place of Cinemeta's own catalogs (Cinemeta's
    // animation shelf stays). Its titles keep Cinemeta as their add-on (see bp_titles).
    if better_posters_on() {
        if let Some(cinemeta) = raw.iter().find(|r| r.addon_id == "com.linvo.cinemeta").map(|r| r.addon_url.clone()) {
            raw.retain(|r| r.addon_id != "com.linvo.cinemeta" || r.extra.is_some());
            let charts = BP_CHARTS.iter().map(|(kind, id, title)| RawRow { key: format!("{BP_ROW}{kind}|{id}"), title: (*title).into(),
                addon_id: "community.betterposters".into(), addon_name: "BetterPosters".into(), addon_url: cinemeta.clone(),
                kind: (*kind).into(), id: (*id).into(), extra: None });
            raw.splice(0..0, charts);
        }
    }
    // New episodes of popular series, after the charts that lead (or Cinemeta's first two shelves).
    let at = raw.iter().position(|r| r.key.starts_with(BP_ROW) && r.id == "trakt-popular").unwrap_or(raw.len().min(2));
    raw.insert(at, RawRow { key: NEW_EPISODES_ROW.into(), title: "New episodes".into(), addon_id: "tvmaze".into(), addon_name: "TVmaze".into(),
        addon_url: CINEMETA.into(), kind: "series".into(), id: "new-episodes".into(), extra: None });
    let shared: Vec<String> = raw.iter().map(|r| r.title.clone()).filter(|t| {
        let mut names: Vec<&str> = raw.iter().filter(|r| &r.title == t).map(|r| r.addon_name.as_str()).collect();
        names.sort(); names.dedup(); names.len() > 1
    }).collect();
    let mut seen = std::collections::HashSet::new();
    Ok(raw.into_iter().map(|r| {
        let title = if seen.insert(format!("{}|{}", r.title, r.addon_name)) { r.title.clone() }
            else { format!("{} {}", r.title, group_label(&r.kind).to_lowercase()) };
        let subtitle = if r.addon_id == "com.linvo.cinemeta" && r.extra.as_ref().is_some_and(|(_, v)| v == "Animation") {
            Some("Cinemeta · Animated series, including anime".to_owned())
        } else if r.key == NEW_EPISODES_ROW {
            Some("Out in the last three days · TVmaze".to_owned())
        } else if shared.contains(&r.title) && !r.addon_name.is_empty() { Some(r.addon_name.clone()) } else { None };
        let ranked = r.key.starts_with(BP_ROW) && bp_ranked(&r.id);
        CatalogRow { key: r.key, title, subtitle, addon_url: r.addon_url, kind: r.kind, id: r.id, extra: r.extra, ranked }
    }).collect())
}

pub async fn catalog(row: CatalogRow) -> Result<Vec<Media>> {
    // A BetterPosters chart; Cinemeta's popular list of the same type when it can't answer.
    if row.key.starts_with(BP_ROW) {
        let mut titles = bp_get(format!("catalog/{}/{}.json", row.kind, row.id)).await.map(|v| bp_titles(&v, &row.addon_url)).unwrap_or_default();
        if row.ranked { titles.truncate(10); }
        if !titles.is_empty() { return Ok(titles); }
        return Box::pin(catalog(CatalogRow { key: String::new(), id: "top".into(), ranked: false, ..row })).await;
    }
    if row.key == NEW_EPISODES_ROW {
        let titles = new_episodes().await;
        anyhow::ensure!(!titles.is_empty(), "TVmaze can't be reached right now");
        return Ok(titles);
    }
    let mut req = json!({"op":"catalog","addon_url":row.addon_url,"type":row.kind,"id":row.id});
    if let Some((k, v)) = &row.extra { req["extra"] = json!({ k.as_str(): v }); }
    let v = exec(req).await?;
    if let Some(e) = v.get("error").and_then(Value::as_str) {
        if v["metas"].as_array().is_none_or(|m| m.is_empty()) { anyhow::bail!("{e}"); }
    }
    let mut out = metas(&v, &row.addon_url);
    let mut keys = std::collections::HashSet::new();
    out.retain(|m| keys.insert(m.key()));
    Ok(out)
}

/// The library op answers `{"items": [...]}`, not catalog-style `metas`.
pub async fn library() -> Result<Vec<Media>> {
    let v = exec(json!({"op":"library"})).await?;
    Ok(v["items"].as_array().into_iter().flatten().filter_map(|m| Media::from_json(m, "")).collect())
}

/// Started, not finished: the Continue watching shelf and the marquee's first choice.
pub fn in_progress(m: &Media) -> bool {
    // A series may have been marked complete before another season arrived. Its current
    // episode's completion state controls resume, rather than that older title-level flag.
    m.position_ms > 0 && !m.video_watched && (m.kind != "movie" || !m.watched)
}

pub fn progress_fraction(m: &Media) -> f64 {
    if m.duration_ms > 0 { (m.position_ms as f64 / m.duration_ms as f64).clamp(0.0, 1.0) } else { 0.0 }
}

fn remaining_label(m: &Media) -> Option<String> {
    let left = m.duration_ms - m.position_ms;
    if m.duration_ms <= 0 || left <= 0 { return None; }
    let minutes = (left + 30_000) / 60_000;
    Some(if minutes >= 60 { format!("{} h {} min left", minutes / 60, minutes % 60) } else { format!("{minutes} min left") })
}

/// "S2 E5, 23 min left", else "40% complete", else "In progress" (resumeContext in Kotlin).
pub fn resume_context(m: &Media) -> String {
    let episode = m.last_video_id.as_ref().and_then(|id| m.raw["videos"].as_array()?.iter().find(|v| v["id"] == id.as_str()).cloned())
        .map(|v| episode_tag(v["season"].as_i64().unwrap_or(1), v["episode"].as_i64().unwrap_or(0)));
    let parts: Vec<String> = [episode, remaining_label(m)].into_iter().flatten().collect();
    if !parts.is_empty() { return parts.join(", "); }
    if m.duration_ms > 0 {
        let f = progress_fraction(m);
        let pct = (f * 100.0).round() as i64;
        format!("{}% complete", if f > 0.0 { pct.max(1) } else { pct })
    } else { "In progress".into() }
}

enum ProgressWrite { Update(Value), Barrier(tokio::sync::oneshot::Sender<()>) }
static PROGRESS: OnceLock<tokio::sync::mpsc::UnboundedSender<ProgressWrite>> = OnceLock::new();

/// Playback completion refreshes Picks only after all preceding progress writes are stored.
pub(crate) async fn flush_progress() {
    if let Some(queue) = PROGRESS.get() {
        let (tx,rx) = tokio::sync::oneshot::channel();
        if queue.send(ProgressWrite::Barrier(tx)).is_ok() { let _ = rx.await; }
    }
    remote_db::trigger_profile_sync_now();
}

/// Save where playback is and how much was really played since the last report (`watched_ms`):
/// the core marks a film or episode watched from that (80% played and 90% reached), never from
/// a seek. Reports are written one after another, in order, on the core runtime. `with_meta`
/// sends the title's metadata (the player's first and last report); the others only merge.
pub fn record_progress(media: Media, video_id: Option<String>, position_ms: i64, duration_ms: i64, watched_ms: i64, with_meta: bool) {
    let Some(runtime) = RUNTIME.get() else { return };
    if duration_ms <= 0 { return; }
    let queue = PROGRESS.get_or_init(|| {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ProgressWrite>();
        runtime.spawn(async move { while let Some(write) = rx.recv().await {
            match write {
                ProgressWrite::Update(req) => {
                    let _ = backend().core.execute(req).await;
                }
                ProgressWrite::Barrier(done) => { let _ = done.send(()); }
            }
        } });
        tx
    });
    let mut req = json!({"op":"progress","type":media.kind,"id":media.id,"addon_url":media.addon_url,
        "position_ms":position_ms.max(0),"duration_ms":duration_ms,"watched_add_ms":watched_ms.clamp(0, duration_ms),
        "meta": if with_meta { media.raw.clone() } else { json!({}) }});
    if let Some(v) = &video_id { req["video_id"] = json!(v); }
    let _ = queue.send(ProgressWrite::Update(req));
}


/// Mark episodes watched or not (without moving where you are in the series).
pub async fn set_episodes_watched(media: &Media, video_ids: Vec<String>, watched: bool) -> Result<()> {
    if video_ids.is_empty() { return Ok(()); }
    exec(json!({"op":"mark_videos","type":media.kind,"id":media.id,"addon_url":media.addon_url,"watched":watched,"video_ids":&video_ids,"meta":media.raw})).await?;
    if !watched {
        remote_db::queue_episodes_unwatched_sync(media, &video_ids);
    }
    remote_db::trigger_profile_sync();
    Ok(())
}

/// Stop tracking a title: its history, progress, marks and rating are deleted from the store.
pub async fn forget(media: &Media) -> Result<()> {
    exec(json!({"op":"forget","type":media.kind,"id":media.id,"addon_url":media.addon_url})).await?;
    remote_db::queue_media_forget_tombstone(media);
    remote_db::trigger_profile_sync();
    Ok(())
}

/// Episodes of a series you watched: marked, or played through.
pub async fn watched_episodes(media: &Media) -> HashSet<String> {
    exec(json!({"op":"watched_videos","type":media.kind,"id":media.id})).await.ok()
        .and_then(|v| v["video_ids"].as_array().map(|ids| ids.iter().filter_map(Value::as_str).map(str::to_owned).collect()))
        .unwrap_or_default()
}

/// "S2 E5" for "tt123:2:5" and "tmdb:123:2:5" (scheme, id, season, episode), "E5" for Kitsu's
/// "kitsu:123:5"; None for anything else, such as "mal:5114:3" (its label comes from the
/// episode list).
pub fn episode_label(video_id: &str) -> Option<String> {
    let parts: Vec<&str> = video_id.split(':').collect();
    let se = |s: &str, e: &str| match (s.parse::<i64>(), e.parse::<i64>()) { (Ok(s), Ok(e)) => Some(episode_tag(s, e)), _ => None };
    match parts.as_slice() {
        ["kitsu", _, e] => e.parse::<i64>().ok().map(|e| format!("E{e}")),
        [series, s, e] if series.starts_with("tt") => se(s, e),
        [_, _, s, e] => se(s, e),
        _ => None,
    }
}

/// "S2 E5": how an episode is named everywhere in the app.
pub fn episode_tag(season: i64, episode: i64) -> String { format!("S{season} E{episode}") }

impl Episode {
    /// "S2 E5".
    pub fn label(&self) -> String { episode_tag(self.season, self.number) }
}

/// The packaged legal player test (same title and stream as demoMedia() in Models.kt).
pub const DEMO_STREAM: &str = "https://media.w3.org/2010/05/sintel/trailer.mp4";

// ---------------------------------------------------------------- title page

#[derive(Clone, Debug, PartialEq)]
pub struct Episode {
    pub id: String, pub season: i64, pub number: i64, pub title: String,
    pub thumbnail: String, pub overview: String, pub released: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct Trailer { pub youtube: Option<String>, pub url: Option<String> }

/// Everything the title's add-on says about it; absent fields stay empty, never invented.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct TitleMeta {
    pub genres: Vec<String>,
    pub logo: String,
    pub runtime: String,
    pub rating: String,
    pub released: String,
    pub country: String,
    pub language: String,
    pub awards: String,
    pub website: String,
    pub directors: Vec<String>,
    pub writers: Vec<String>,
    pub cast: Vec<String>,
    pub trailers: Vec<Trailer>,
    pub episodes: Vec<Episode>,
    /// The synopsis, for titles whose catalog entry had none.
    pub description: String,
}

fn names(meta: &Value, field: &str, link_category: &str) -> Vec<String> {
    let mut out: Vec<String> = meta[field].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
    for l in meta["links"].as_array().into_iter().flatten() {
        if l["category"].as_str().is_some_and(|c| c.eq_ignore_ascii_case(link_category)) {
            if let Some(n) = l["name"].as_str() { out.push(n.to_owned()); }
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|n| !n.trim().is_empty() && seen.insert(n.to_lowercase()));
    out
}

/// "2014-11-05T00:00:00.000Z" -> "5 Nov 2014"; anything else is shown as the add-on wrote it.
pub fn date_label(raw: &str) -> String {
    let d = raw.get(..10).unwrap_or(raw);
    let parts: Vec<&str> = d.split('-').collect();
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    match parts.as_slice() {
        [y, m, day] if y.len() == 4 => match (m.parse::<usize>(), day.parse::<u32>()) {
            (Ok(m @ 1..=12), Ok(day)) => format!("{day} {} {y}", MONTHS[m - 1]),
            _ => raw.to_owned(),
        },
        _ => raw.to_owned(),
    }
}

fn episodes_from_meta(meta: &Value) -> Vec<Episode> {
    let mut episodes: Vec<Episode> = meta["videos"].as_array().into_iter().flatten().filter_map(|e| {
        let id = s(e, "id");
        (!id.is_empty()).then(|| Episode {
            id, season: e["season"].as_i64().unwrap_or(1), number: e["episode"].as_i64().unwrap_or(0),
            title: Some(s(e, "title")).filter(|t| !t.is_empty()).unwrap_or_else(|| s(e, "name")),
            thumbnail: s(e, "thumbnail"),
            overview: Some(s(e, "overview")).filter(|t| !t.is_empty()).unwrap_or_else(|| s(e, "description")),
            released: s(e, "released"),
        })
    }).collect();
    episodes.sort_by_key(|e| (if e.season == 0 { i64::MAX } else { e.season }, e.number));
    let mut seen = HashSet::new();
    episodes.retain(|e| seen.insert(e.id.clone()));
    episodes
}

pub(super) static TITLE_META_CACHE: OnceLock<Mutex<HashMap<String, TitleMeta>>> = OnceLock::new();
static TITLE_PREFETCHING: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

/// Start warming `title_artwork_fast` and `title_meta` the instant a finger touches a card
/// (`onpointerdown`), gaining a 100-160ms head start before `onclick` even mounts `TitlePage`.
pub fn prefetch_title(media: &Media) {
    if cached_title_meta(media).is_some() && cached_title_artwork(media).is_some() {
        return;
    }
    let key = media.key();
    {
        let Ok(mut set) = TITLE_PREFETCHING.get_or_init(Default::default).lock() else { return };
        if set.len() > 64 { set.clear(); }
        if !set.insert(key.clone()) { return; }
    }
    let m = media.clone();
    let Some(rt) = RUNTIME.get() else { return };
    rt.spawn(async move {
        let _ = tokio::join!(title_artwork_fast(&m), title_meta(&m));
        if let Ok(mut set) = TITLE_PREFETCHING.get_or_init(Default::default).lock() {
            set.remove(&key);
        }
    });
}

/// Synchronously read cached `TitleMeta` from this session so re-opening a title renders
/// immediately on Frame 0 with zero loading flash.
pub fn cached_title_meta(media: &Media) -> Option<TitleMeta> {
    TITLE_META_CACHE.get()?.lock().ok()?.get(&media.key()).cloned()
}

pub async fn title_meta(media: &Media) -> Result<TitleMeta> {
    if let Some(hit) = cached_title_meta(media) {
        return Ok(hit);
    }
    let request = async {
        let primary = exec(json!({"op":"meta","type":media.kind,"id":media.id,"addon_url":media.addon_url})).await;
        if primary.is_ok() {
            return primary;
        }
        if let Some(imdb) = remote_db::extract_imdb_id(&media.id, &media.raw) {
            if media.addon_url != CINEMETA || media.id != imdb {
                if let Ok(fallback) = exec(json!({"op":"meta","type":media.kind,"id":imdb,"addon_url":CINEMETA})).await {
                    return Ok(fallback);
                }
            }
        }
        primary
    };
    // Fetch add-on meta and BetterPosters overlay concurrently — do not block title details on
    // slower artwork/TVmaze/TMDB calls (which `use_title_artwork` loads independently).
    let (addon, bp) = tokio::join!(request, bp_meta(media));
    let meta = match (addon, bp) {
        (Ok(v), bp) => merge_meta(v.get("meta").cloned().unwrap_or(v), bp.as_ref()),
        // A title Cinemeta doesn't know yet: BetterPosters alone.
        (Err(_), Some(bp)) => bp,
        (Err(e), None) => return Err(e),
    };
    let meta = &meta;
    let episodes = episodes_from_meta(meta);
    let mut trailers: Vec<Trailer> = Vec::new();
    for t in meta["trailerStreams"].as_array().into_iter().flatten() {
        trailers.push(Trailer { youtube: Some(s(t, "ytId")).filter(|x| !x.is_empty()), url: Some(s(t, "url")).filter(|x| x.starts_with("https://")) });
    }
    for t in meta["trailers"].as_array().into_iter().flatten() {
        if t["type"].as_str().is_none_or(|k| k == "Trailer") {
            trailers.push(Trailer { youtube: Some(s(t, "source")).filter(|x| !x.is_empty()), url: None });
        }
    }
    trailers.retain(|t| t.youtube.is_some() || t.url.is_some());
    let mut seen_trailers = HashSet::new();
    trailers.retain(|t| seen_trailers.insert(t.clone()));
    let genres: Vec<String> = meta["genres"].as_array().into_iter().flatten().filter_map(Value::as_str).map(canonical_genre).collect();
    let out = TitleMeta {
        genres,
        logo: s(meta, "logo"),
        runtime: s(meta, "runtime"),
        rating: s(meta, "imdbRating"),
        released: Some(s(meta, "released")).filter(|r| !r.is_empty()).map(|r| date_label(&r)).unwrap_or_default(),
        country: s(meta, "country"),
        language: s(meta, "language"),
        awards: s(meta, "awards"),
        website: Some(s(meta, "website")).filter(|w| w.starts_with("https://")).unwrap_or_default(),
        directors: names(meta, "director", "Directors"),
        writers: names(meta, "writer", "Writers"),
        cast: names(meta, "cast", "Cast"),
        trailers,
        episodes,
        description: s(meta, "description"),
    };
    if let Ok(mut c) = TITLE_META_CACHE.get_or_init(Default::default).lock() {
        cache_insert(&mut c, 200, media.key(), out.clone());
    }
    Ok(out)
}

/// What the store knows about a title opened from anywhere (a catalog shelf, Search, Picks):
/// saved, watched, liked and progress come from there, the rest from the add-on.
pub async fn stored(media: &Media) -> Option<Media> {
    let v = exec(json!({"op":"item","type":media.kind,"id":media.id,"addon_url":media.addon_url})).await.ok()?;
    Media::from_json(&v["item"], &media.addon_url)
}

/// Save a title to the Library, or take it out (its history stays).
pub async fn set_saved(media: &Media, saved: bool) -> Result<()> {
    exec(json!({"op":"set_library","type":media.kind,"id":media.id,"addon_url":media.addon_url,"saved":saved,"meta":media.raw})).await?;
    remote_db::trigger_profile_sync();
    Ok(())
}

/// Mark a film watched or not: the core's own mutation.
pub async fn set_watched(media: &Media, watched: bool) -> Result<()> {
    exec(json!({"op":"mark_watched","type":media.kind,"id":media.id,"addon_url":media.addon_url,"watched":watched,"meta":media.raw})).await?;
    remote_db::trigger_profile_sync();
    Ok(())
}

/// -1 not for me, 0 clear, 1 like.
pub async fn feedback(media: &Media, value: i64) -> Result<()> {
    exec(json!({"op":"feedback","type":media.kind,"id":media.id,"addon_url":media.addon_url,
        "value":value,"meta":media.raw})).await?;
    remote_db::trigger_profile_sync();
    Ok(())
}


// ---------------------------------------------------------------- search

#[derive(Clone, Debug, PartialEq, Default)]
pub struct SearchOutcome {
    pub results: Vec<(String, Media)>,
    pub failed: Vec<String>,
    pub catalogs: usize,
}

pub async fn search(query: String) -> Result<SearchOutcome> {
    let v = exec(json!({"op":"search","query":query})).await?;
    let mut out = SearchOutcome::default();
    let mut seen = HashSet::new();
    for group in v.get("groups").and_then(Value::as_array).into_iter().flatten() {
        if retired_addon(&s(group, "addon_url")) { continue; }
        out.catalogs += 1;
        let addon = s(group, "addon_name");
        if group.get("error").is_some() && !out.failed.contains(&addon) { out.failed.push(addon.clone()); }
        // The first add-on to list a title keeps it.
        for m in metas(group, &s(group, "addon_url")) {
            if seen.insert(m.key()) { out.results.push((addon.clone(), m)); }
        }
    }
    out.results = rank_search_results(&query, out.results);
    Ok(out)
}

/// Whether a release names this title: its file name, or a line of its torrent name, starts with
/// the title's words (leading "[site]" and "www.site - " tags and a first "The" aside) and has
/// more after them; for a film, a year right after the title is within one of `year`. A line
/// that is only the title says nothing (some add-ons print the requested title on its own line).
/// A translated title may come first: "Kód 3 - Code 3 (2024)", "Код 3 / Code 3 (2025)".
pub fn names_title(s: &Source, title: &str, year: Option<i32>) -> bool {
    fn words(text: &str) -> Vec<String> {
        let mut t = text.trim();
        loop {
            if let Some(rest) = t.strip_prefix('[').and_then(|r| r.split_once(']')).map(|(_, r)| r.trim_start()) { t = rest; continue; }
            if t.get(..4).is_some_and(|p| p.eq_ignore_ascii_case("www.")) { if let Some((_, rest)) = t.split_once(" - ") { t = rest.trim_start(); continue; } }
            break;
        }
        // "Schindler's" and "Schindlers", "Fast & Furious" and "Fast and Furious" are the same words.
        let t = t.replace(['\'', '\u{2019}', '`'], "").replace('&', " and ");
        let mut w = search_words(&t);
        if w.first().is_some_and(|f| f == "the") { w.remove(0); }
        w
    }
    let wanted = words(title);
    if wanted.is_empty() { return true; }
    let file = s.filename.rsplit(['/', '\\']).next().unwrap_or("");
    // Each line as it is, and what follows a " - " or " / " in it (after a translated title).
    let lines = std::iter::once(file).chain(s.description.lines()).flat_map(|line| {
        std::iter::once(line).chain([" - ", " / "].into_iter().filter_map(move |sep| line.split_once(sep).map(|(_, rest)| rest)))
    });
    lines.into_iter().any(|line| {
        let w = words(line);
        if w.len() <= wanted.len() || w[..wanted.len()] != wanted[..] { return false; }
        // What follows the title must not make it another one ("The Dark Knight Rises"): a year
        // (the film's, give or take one), a season or episode, or a release tag.
        let next = w[wanted.len()].as_str();
        match next.parse::<i32>().ok().filter(|y| (1900..2100).contains(y)) {
            Some(got) => year.is_none_or(|want| (want - got).abs() <= 1),
            None => release_word().is_match(next)
                || (year.is_none() && (2..=3).contains(&next.len()) && next.bytes().all(|b| b.is_ascii_digit())),
        }
    })
}
static_re!(release_word, r"^(?:s\d{1,2}(?:e\d{1,3})?|ep?\d{1,3}|ep|bd|\d{1,2}x\d{2,3}|\d{3,4}x\d{3,4}|v\d|season|series|complete|\d{3,4}p|4k|8k|uhd|fhd|hd|sdr|hdr\d*|hdr10plus|dv|dovi|bluray|blu|bdrip|brrip|bdremux|remux|web|webdl|webrip|dl|hdtv|imax|extended|unrated|uncut|remastered|theatrical|directors|criterion|proper|repack|hybrid|multi|dual|x264|x265|h264|h265|hevc|avc|av1|10bit|ita|eng|fre|french|ger|german|spa|spanish|latino|rus|jpn|kor|us|uk)$");

fn search_words(text: &str) -> Vec<String> {
    let folded: String = text.to_lowercase().nfd().filter(|c| !is_combining_mark(*c)).collect();
    folded.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_owned).collect()
}

/// 0 exact, 1 starts with it, 2 every word starts a title word, 3 contains it,
/// 4 contains every word, 5 matched only by add-on metadata (same as SearchPanel.kt).
pub fn search_rank(query: &str, name: &str) -> u8 {
    let q = search_words(query);
    let n = search_words(name);
    if q.is_empty() { return 5; }
    let qt = q.join(" ");
    let nt = n.join(" ");
    if nt == qt { 0 }
    else if nt.starts_with(&qt) { 1 }
    else if q.iter().all(|w| n.iter().any(|x| x.starts_with(w.as_str()))) { 2 }
    else if nt.contains(&qt) { 3 }
    else if q.iter().all(|w| nt.contains(w.as_str())) { 4 }
    else { 5 }
}

/// Best title match first; add-on order is kept within a rank (stable sort).
pub fn rank_search_results(query: &str, mut results: Vec<(String, Media)>) -> Vec<(String, Media)> {
    results.sort_by_key(|(_, m)| search_rank(query, &m.name));
    results
}

// ---------------------------------------------------------------- history of viewing

/// One title you watched or started, and when you last did.
#[derive(Clone, Debug, PartialEq)]
pub struct Seen { pub media: Media, pub at: i64, pub episode: Option<String> }

/// Everything watched or started, latest first (the core's history, with when you last watched).
pub async fn viewing_history() -> Result<Vec<Seen>> { recent_viewing(600).await }

/// The latest `limit` entries of `viewing_history` (fewer when some are add-on bookkeeping rows).
pub async fn recent_viewing(limit: usize) -> Result<Vec<Seen>> {
    let v = exec(json!({"op":"history","limit":limit})).await?;
    let mut out = Vec::new();
    for item in v["items"].as_array().into_iter().flatten() {
        let Some(media) = Media::from_json(item, "") else { continue };
        if media.kind == "other" { continue; } // add-on bookkeeping rows (e.g. a provider "Downloads" folder)
        let episode = media.last_video_id.as_deref().and_then(episode_label).filter(|_| media.kind != "movie");
        out.push(Seen { at: item["seen"].as_i64().unwrap_or(0), media, episode });
    }
    Ok(out)
}

/// Unix seconds now.
pub fn now_secs() -> i64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0) }

/// Calendar date (UTC) from unix seconds: (year, month 1-12, day, weekday 0 = Monday).
pub fn civil(secs: i64) -> (i64, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let weekday = ((days + 3).rem_euclid(7)) as u32; // 1970-01-01 was a Thursday
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + if m <= 2 { 1 } else { 0 }, m, d, weekday)
}

pub fn month_name(m: u32) -> &'static str {
    ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"][(m.clamp(1, 12) - 1) as usize]
}

/// "Today", "Yesterday", a weekday within the week, else "12 Oct".
pub fn when_label(at: i64, now: i64) -> String {
    let (day_at, day_now) = (at.div_euclid(86_400), now.div_euclid(86_400));
    match day_now - day_at {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        2..=6 => ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"][civil(at).3 as usize].into(),
        _ => { let (_, m, d, _) = civil(at); format!("{d} {}", &month_name(m)[..3]) }
    }
}

// ---------------------------------------------------------------- accent

/// The accent a title lends the interface, as CSS hex; None keeps Bone.
pub async fn accent(url: String) -> Option<String> {
    if url.is_empty() { return None; }
    if let Some(hit) = backend().accents.lock().ok()?.get(&url).cloned() { return hit; }
    let key = url.clone();
    // 48×48 pixels are all that is read: TMDB's small size is a tenth of the download.
    let url = small_tmdb_image(&url).unwrap_or(url);
    let computed = on_runtime(async move {
        const LIMIT: usize = 12_000_000;
        let mut response = backend().http.get(&url).send().await.ok()?.error_for_status().ok()?;
        if response.content_length().is_some_and(|n| n > LIMIT as u64) { return None; }
        // Read at most LIMIT bytes whatever the server says, and decode within a memory budget.
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.ok()? {
            if bytes.len() + chunk.len() > LIMIT { return None; }
            bytes.extend_from_slice(&chunk);
        }
        // Offload CPU-intensive image decoding & thumbnailing off the Tokio async I/O workers.
        tokio::task::spawn_blocking(move || {
            let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
            let mut limits = image::Limits::default();
            limits.max_alloc = Some(256 * 1024 * 1024);
            limits.max_image_width = Some(12_000);
            limits.max_image_height = Some(12_000);
            reader.limits(limits);
            let img = reader.decode().ok()?.thumbnail(48, 48).to_rgba8();
            let pixels: Vec<[u8; 4]> = img.pixels().map(|p| p.0).collect();
            artwork_accent(&pixels).map(|(r, g, b)| format!("#{r:02x}{g:02x}{b:02x}"))
        }).await.ok().flatten()
    }).await.ok().flatten();
    if let Ok(mut cache) = backend().accents.lock() { cache_insert(&mut cache, 2000, key, computed.clone()); }
    computed
}

/// The same TMDB image at 300 px wide (TMDB serves every image at that size), for reading colours.
fn small_tmdb_image(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://image.tmdb.org/t/p/")?;
    let (size, path) = rest.split_once('/')?;
    (size != "w300").then(|| format!("https://image.tmdb.org/t/p/w300/{path}"))
}

/// Port of ArtworkAccent.kt: the most vivid well-lit colour, lifted until black
/// text on it (and it on black) reaches 4.5:1. None for greyscale artwork.
pub fn artwork_accent(pixels: &[[u8; 4]]) -> Option<(u8, u8, u8)> {
    let mut best = None;
    let mut best_score = 0.0;
    for p in pixels {
        if p[3] < 128 { continue; }
        let (r, g, b) = (p[0] as f64 / 255.0, p[1] as f64 / 255.0, p[2] as f64 / 255.0);
        let high = r.max(g).max(b);
        let low = r.min(g).min(b);
        if high < 0.25 { continue; }
        let saturation = (high - low) / high;
        let score = saturation * saturation * high;
        if score > best_score { best_score = score; best = Some((r, g, b)); }
    }
    let (mut r, mut g, mut b) = best.filter(|_| best_score >= 0.08)?;
    for _ in 0..20 {
        if luminance(r, g, b) >= 0.175 { break; }
        r += (1.0 - r) * 0.12;
        g += (1.0 - g) * 0.12;
        b += (1.0 - b) * 0.12;
    }
    Some(((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8))
}

fn luminance(r: f64, g: f64, b: f64) -> f64 {
    let c = |x: f64| if x <= 0.03928 { x / 12.92 } else { ((x + 0.055) / 1.055).powf(2.4) };
    0.2126 * c(r) + 0.7152 * c(g) + 0.0722 * c(b)
}

#[cfg(test)]
mod review_tests {
    #[test]
    fn accents_read_tmdb_images_small() {
        assert_eq!(super::small_tmdb_image("https://image.tmdb.org/t/p/w1280/abc.jpg").as_deref(), Some("https://image.tmdb.org/t/p/w300/abc.jpg"));
        assert_eq!(super::small_tmdb_image("https://image.tmdb.org/t/p/original/abc.jpg").as_deref(), Some("https://image.tmdb.org/t/p/w300/abc.jpg"));
        assert_eq!(super::small_tmdb_image("https://image.tmdb.org/t/p/w300/abc.jpg"), None);
        assert_eq!(super::small_tmdb_image("https://images.metahub.space/background/medium/tt1/img"), None);
    }

    use super::*;

    #[test]
    fn track_languages_match_the_setting() {
        assert!(track_in_language("ita", "italian"));
        assert!(track_in_language("it-IT", "italian"));
        assert!(!track_in_language("eng", "italian"));
        assert!(!track_in_language("", "italian"));
        assert!(track_in_language("es-419", "latino"));
        assert!(!track_in_language("es-ES", "latino"));
        assert!(track_in_language("pt-BR", "portuguese"));
        assert_eq!(track_codes("italian").join(","), "it,ita");
        // Subtitle add-on codes.
        assert!(track_in_language("pob", "portuguese"));
        assert!(track_in_language("spl", "latino"));
        assert!(track_in_language("nb", "norwegian"));
    }

    #[test]
    fn skip_times_come_from_introdb_and_aniskip() {
        use crate::engine::{Segment, SegmentKind::*};
        assert_eq!(skip_lookup("tt0903747:1:2"), Some(SkipLookup::IntroDb { imdb: "tt0903747".into(), season: 1, episode: 2 }));
        assert_eq!(skip_lookup("kitsu:1376:5"), Some(SkipLookup::Kitsu { id: 1376, episode: 5 }));
        assert_eq!(skip_lookup("mal:5114:3"), Some(SkipLookup::Mal { id: 5114, episode: 3 }));
        assert_eq!(skip_lookup("tt0903747"), None);
        assert_eq!(skip_lookup("kitsu:1376"), None);
        assert_eq!(skip_lookup("tt:1:2"), None);
        // IntroDB's answer, as the service sends it.
        let intro = json!({"imdb_id":"tt0903747","intro":{"start_sec":60,"end_sec":95,"start_ms":60000,"end_ms":95500},"recap":null,
            "outro":{"start_sec":3431,"end_sec":3500,"start_ms":3431000,"end_ms":3500000},"no_intro":false});
        assert_eq!(introdb_segments(&intro, 3480.0), vec![Segment { kind: Intro, start: 60.0, end: 95.5 }]);
        assert_eq!(introdb_segments(&intro, 3502.0).len(), 2);
        assert_eq!(introdb_segments(&json!({"outro":{"start_ms":3000000,"end_ms":null}}), 3100.0), vec![Segment { kind: Credits, start: 3000.0, end: 3100.0 }]);
        assert!(introdb_segments(&Value::Null, 100.0).is_empty());
        // AniSkip's, and a Kitsu mapping.
        let ani = json!({"found":true,"results":[{"interval":{"startTime":1349.903,"endTime":1474},"skipType":"ed"},
            {"interval":{"startTime":58.642,"endTime":148.642},"skipType":"op"},{"interval":{"startTime":1,"endTime":2},"skipType":"recap"}]});
        assert_eq!(aniskip_segments(&ani, 1474.6), vec![Segment { kind: Intro, start: 58.642, end: 148.642 }, Segment { kind: Credits, start: 1349.903, end: 1474.0 }]);
        assert_eq!(mal_of(&json!({"data":[{"attributes":{"externalSite":"myanimelist/anime","externalId":"1535"}}]})), Some(1535));
        assert_eq!(mal_of(&json!({"data":[]})), None);
    }
    #[test]
    fn episode_labels_for_series_and_kitsu() {
        assert_eq!(episode_label("tt123:2:5").as_deref(), Some("S2 E5"));
        assert_eq!(episode_label("kitsu:12345:5").as_deref(), Some("E5"));
        assert_eq!(episode_label("tt123"), None);
        assert_eq!(episode_label("custom:abc:x"), None);
        assert_eq!(episode_label("mal:5114:3"), None);
        assert_eq!(episode_label("tmdb:123:1:2").as_deref(), Some("S1 E2"));
    }

    #[test]
    fn catalogs_needing_an_extra_are_not_shelves() {
        assert!(catalog_needs_extra(&json!({"extra":[{"name":"genre","isRequired":true}]})));
        assert!(catalog_needs_extra(&json!({"extraRequired":["search"]})));
        assert!(!catalog_needs_extra(&json!({"extra":[{"name":"skip"}],"extraRequired":[]})));
    }

    #[test]
    fn audio_language_names_its_subtitles() {
        assert_eq!(subtitle_language_of("latino"), "Spanish (Latin America)");
        assert_eq!(subtitle_language_of("italian"), "Italian");
        assert_eq!(subtitle_language_of("nope"), "");
    }
}

#[cfg(test)]
mod subtitle_tests {
    use super::*;
    fn sub(id: &str, code: &str) -> OnlineSubtitle {
        subtitle_of(&json!({"id": id, "url": format!("https://subs.example/{id}"), "lang": code})).unwrap()
    }

    #[test]
    fn preferred_language_then_english_then_by_name() {
        let list = vec![sub("1", "spa"), sub("2", "eng"), sub("3", "ita"), sub("4", "xx"), sub("5", "ita"), sub("6", "fre"), sub("2", "eng")];
        let ranked = rank_subtitles(list, Some("Italian"));
        let ids: Vec<&str> = ranked.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["3", "5", "2", "6", "1", "4"]);
        assert_eq!(ranked[5].language, "XX");
        assert_eq!(rank_subtitles(vec![sub("1", "spa"), sub("2", "eng")], None)[0].language, "English");
    }

    #[test]
    fn search_by_language_name_or_code() {
        let br = sub("1", "pob");
        assert_eq!(br.language, "Portuguese (Brazil)");
        for q in ["", "port", "Brazil", "pob", "  PORTUGUESE "] { assert!(subtitle_matches(&br, q), "{q}"); }
        for q in ["ita", "razil", "po b"] { assert!(!subtitle_matches(&br, q), "{q}"); }
        assert!(subtitle_matches(&sub("2", "ita"), "it"));
    }

    #[test]
    fn keeps_the_release_name_and_skips_files_without_a_link() {
        let s = subtitle_of(&json!({"id": "9", "url": "https://subs.example/9", "lang": "eng", "subtitleFileName": "Film.2019.1080p.srt"})).unwrap();
        assert_eq!(s.release.as_deref(), Some("Film.2019.1080p.srt"));
        assert!(subtitle_of(&json!({"id": "9", "url": "file:///etc/passwd", "lang": "eng"})).is_none());
        assert!(subtitle_of(&json!({"id": "9", "url": "http://subs.example/plain.srt", "lang": "eng"})).is_none());
        assert!(subtitle_of(&json!({"id": "9", "url": "https://127.0.0.1:8443/local.srt", "lang": "eng"})).is_none());
        assert!(subtitle_of(&json!({"id": "9", "url": "https://localhost/local.srt", "lang": "eng"})).is_none());
        assert!(subtitle_of(&json!({"id": "9", "lang": "eng"})).is_none());
        // `episodes_from_meta` deduplicates non-adjacent duplicate IDs.
        let eps = episodes_from_meta(&json!({"videos": [
            {"id": "tt1:1:1", "season": 1, "episode": 1, "title": "First"},
            {"id": "tt1:1:2", "season": 1, "episode": 2, "title": "Second"},
            {"id": "tt1:1:1", "season": 2, "episode": 1, "title": "Duplicate non-adjacent"}
        ]}));
        assert_eq!(eps.len(), 2);
        // `names_title` recognizes anime bare episode numbers, `ep02`, `bd`, `1920x1080`, `v2`.
        let mk = |t: &str| source_of("A", &json!({"url": "https://x.invalid/v", "title": t}));
        assert!(names_title(&mk("[SubsPlease] Frieren - 02 (1080p).mkv"), "Frieren", None));
        assert!(names_title(&mk("Frieren - Ep02 [BD 1920x1080].mkv"), "Frieren", None));
        assert!(names_title(&mk("Frieren v2 1080p.mkv"), "Frieren", None));
        assert!(!names_title(&mk("Frieren 02 1080p.mkv"), "Frieren", Some(2023)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_production_from_one_tmdb_answer() {
        let movie = json!({"tagline": "A tagline", "original_language": "en", "spoken_languages": [{"iso_639_1": "en", "english_name": "English"}],
            "images": {"backdrops": [{"iso_639_1": null, "file_path": "/a.jpg", "vote_average": 5.0}, {"iso_639_1": null, "file_path": "/b.jpg", "vote_average": 7.0}, {"iso_639_1": "en", "file_path": "/t.jpg", "vote_average": 9.0}]},
            "credits": {"cast": [{"name": "Matt Damon", "character": "Odysseus", "profile_path": "/m.jpg"}],
                "crew": [{"name": "Ludwig Göransson", "job": "Original Music Composer"}, {"name": "Christopher Nolan", "job": "Director"}, {"name": "Christopher Nolan", "job": "Screenplay"}]},
            "production_companies": [{"name": "Universal", "logo_path": "/u.png"}, {"name": "No logo", "logo_path": null}],
            "release_dates": {"results": [{"iso_3166_1": "GB", "release_dates": [{"certification": "12A"}]}, {"iso_3166_1": "US", "release_dates": [{"certification": ""}, {"certification": "PG-13"}]}]},
            "videos": {"results": [{"site": "YouTube", "type": "Featurette", "key": "k1", "name": "Making of"}, {"site": "Vimeo", "type": "Trailer", "key": "v"}]}});
        let a = production(&movie, "movie", Some("https://image.tmdb.org/t/p/w1280/b.jpg"));
        assert_eq!(a.stills, ["https://image.tmdb.org/t/p/w1280/a.jpg"], "textless only, the backdrop not twice");
        assert_eq!(a.tagline.as_deref(), Some("A tagline"));
        assert_eq!(a.cast[0].role, "Odysseus");
        let crew: Vec<(&str, Vec<&str>)> = a.crew.iter().map(|(l, p)| (l.as_str(), p.iter().map(|p| p.name.as_str()).collect())).collect();
        assert_eq!(crew, [("Directed by", vec!["Christopher Nolan"]), ("Written by", vec!["Christopher Nolan"]), ("Music by", vec!["Ludwig Göransson"])]);
        assert_eq!(a.studios.len(), 1);
        assert_eq!(a.certificate.as_deref(), Some("PG-13"), "the US certificate first");
        assert_eq!(a.language.as_deref(), Some("English"));
        assert_eq!(a.videos.len(), 1);
        let show = json!({"created_by": [{"name": "Graham Wagner", "profile_path": "/g.jpg"}], "networks": [{"name": "Prime Video", "logo_path": "/p.png"}],
            "aggregate_credits": {"cast": [{"name": "Ella Purnell", "roles": [{"character": "Lucy MacLean"}], "profile_path": null}]},
            "content_ratings": {"results": [{"iso_3166_1": "DE", "rating": "16"}]}, "seasons": [{"season_number": 1, "poster_path": "/s1.jpg"}, {"season_number": 2, "poster_path": null}]});
        let b = production(&show, "tv", None);
        assert_eq!(b.crew[0].0, "Created by");
        assert_eq!(b.crew[0].1[0].photo.as_deref(), Some("https://image.tmdb.org/t/p/h632/g.jpg"), "a creator's photo, for their card");
        assert_eq!(b.cast[0].role, "Lucy MacLean");
        assert_eq!(b.cast[0].photo, None);
        assert_eq!(b.certificate.as_deref(), Some("16"), "any country when the US has none");
        assert_eq!(b.season_posters.get(&1).map(String::as_str), Some("https://image.tmdb.org/t/p/w500/s1.jpg"));
        assert_eq!(b.studios[0].name, "Prime Video");
        assert_eq!(b.studios[0].logo, "https://image.tmdb.org/t/p/w300/p.png");
    }

    #[test]
    fn better_posters_charts_stay_cinemeta_titles() {
        let v = json!({"metas": [
            {"id": "tt33764258", "type": "movie", "name": "The Odyssey", "poster": "https://btttr.cc/poster/movie/tt33764258/auto~gr.png", "runtime": "173 min"},
            {"id": "tmdb:12", "type": "movie", "name": "TMDB only"},
            {"id": "tt33764258", "type": "movie", "name": "The Odyssey"},
        ]});
        let titles = bp_titles(&v, CINEMETA);
        assert_eq!(titles.len(), 1, "IMDb ids only, once each");
        assert_eq!(titles[0].addon_url, CINEMETA, "keyed like the same title from Cinemeta");
        assert_eq!(titles[0].raw["runtime"], "173 min");
    }

    #[test]
    fn better_posters_metadata_over_the_addons() {
        let addon = json!({"runtime": "1 min", "description": "Cinemeta's", "cast": ["A"], "trailers": [{"source": "x", "type": "Trailer"}],
            "videos": [{"id": "tt1:1:1", "season": 1, "episode": 1, "name": "Pilot", "thumbnail": ""}, {"id": "tt1:1:2", "season": 1, "episode": 2, "name": "Two"}]});
        let bp = json!({"runtime": "44 min", "description": "", "imdbRating": "8.2", "trailers": [{"source": "y", "type": "Trailer"}],
            "videos": [{"id": "tt1:1:1", "title": "Pilot (BP)", "thumbnail": "https://image.tmdb.org/s.jpg"}, {"id": "tt1:9:9", "title": "Not in the add-on"}]});
        let m = merge_meta(addon.clone(), Some(&bp));
        assert_eq!(m["runtime"], "44 min");
        assert_eq!(m["imdbRating"], "8.2");
        assert_eq!(m["description"], "Cinemeta's", "empty BetterPosters fields never erase the add-on's");
        assert_eq!(m["cast"], json!(["A"]));
        assert_eq!(m["trailers"].as_array().map(Vec::len), Some(2));
        assert_eq!(m["trailers"][0]["source"], "y");
        let videos = m["videos"].as_array().unwrap();
        assert_eq!(videos.len(), 2, "the add-on's episode list stays");
        assert_eq!(videos[0]["title"], "Pilot (BP)");
        assert_eq!(videos[0]["thumbnail"], "https://image.tmdb.org/s.jpg");
        assert_eq!(videos[1]["name"], "Two");
        // Nothing from BetterPosters: the add-on's, unchanged. An add-on without episodes takes BetterPosters'.
        assert_eq!(merge_meta(addon.clone(), None), addon);
        assert_eq!(merge_meta(json!({"name": "X"}), Some(&bp))["videos"].as_array().map(Vec::len), Some(2));
    }

    fn media(name: &str, kind: &str, year: &str, genre: &str) -> Media {
        Media::from_json(&json!({"id":name,"name":name,"type":kind,"releaseInfo":year,"genres":[genre]}), "a").unwrap()
    }

    #[test]
    fn tvmaze_show_type_tags_animation() {
        let source = json!({"type":"Animation","genres":["Comedy","Family"]});
        let genres = tvmaze_genres(&source);
        assert!(genres.iter().any(|g| g.eq_ignore_ascii_case("animation")));
        assert!(!tvmaze_genres(&json!({"type":"Scripted","genres":["Drama"]})).contains(&"Animation".into()));
    }
    #[test]
    fn search_ranking_matches_the_android_app() {
        let names = ["Dune", "Godzilla Minus One", "One Piece Film: Red", "One Piece", "The Big Red One"];
        let results = names.iter().map(|n| ("Cinemeta".to_owned(), media(n, "movie", "2020", "Drama"))).collect();
        let ranked: Vec<_> = rank_search_results("one piece", results).into_iter().map(|(_, m)| m.name).collect();
        assert_eq!(ranked, ["One Piece", "One Piece Film: Red", "Dune", "Godzilla Minus One", "The Big Red One"]);
        assert_eq!(search_rank("  ONE piece ", "One Piece"), 0);
        assert_eq!(search_rank("piece one", "One Piece"), 2);
        assert_eq!(search_rank("amelie", "Amélie"), 0);
        assert_eq!(search_rank("one", "Dune"), 5);
    }

    #[test]
    fn recent_queries_are_trimmed_deduplicated_and_bounded() {
        assert_eq!(recent_queries(" Dune ", &["Alien".into(), "dune".into()]), ["Dune", "Alien"]);
        assert_eq!(recent_queries("a", &["Alien".into()]), ["Alien"]);
        assert_eq!(recent_queries("x1", &(0..20).map(|i| format!("q{i}")).collect::<Vec<_>>()).len(), 12);
    }

    #[test]
    fn sources_read_quality_size_and_kind_without_estimating() {
        let st = source_of("A", &json!({"name":"RD 4K","title":"Film.2160p.HDR10+.mkv\n💾 21.4 GB","url":"https://x.invalid/f"}));
        assert_eq!((st.quality, st.hdr, st.kind, st.size.as_deref()), (Some("4K"), Some("HDR10+"), SourceKind::Direct, Some("21.4 GB")));
        let t = source_of("B", &json!({"name":"P2P","title":"Film 1080p","infoHash":"abc"}));
        assert_eq!((t.kind, t.size, t.url.as_str()), (SourceKind::Torrent, None, ""));
        let b = source_of("C", &json!({"url":"https://x.invalid/g","behaviorHints":{"videoSize":1610612736u64,"filename":"g.720p.mp4"}}));
        assert_eq!((b.size.as_deref(), b.quality), (Some("1.5 GiB"), Some("720p")));
    }

    #[test]
    fn season_packs_are_split_even_when_the_file_is_named_for_the_episode() {
        // Multi-line season pack label: the pack name on the first line, the episode's file below, the whole
        // season's size declared as the file's.
        let pack = source_of("A", &json!({"url":"https://x.invalid/p","behaviorHints":{"filename":"Show.S01E01.1080p.BluRay.REMUX.AVC-GRP.mkv","videoSize":119243487977u64},
            "description":"Show.S01.1080p.BluRay.REMUX.AVC-GRP\nShow.S01E01.1080p.BluRay.REMUX.AVC-GRP.mkv\n💾111.05 GiB"}));
        assert!(pack.pack);
        assert_eq!(episode_size(&pack, Some(10)), Some(11_924_348_797));
        // A declared size unlike the pack label is the episode file's own.
        let own = source_of("A", &json!({"url":"https://x.invalid/q","behaviorHints":{"videoSize":9_000_000_000u64},
            "title":"Show.S01.2160p.WEB-DL\nShow.S01E01.mkv\n💾 90 GB"}));
        assert_eq!(episode_size(&own, Some(10)), Some(9_000_000_000));
        let single = source_of("A", &json!({"url":"https://x.invalid/e","title":"Show.S01E01.2160p.WEB-DL\n💾 9 GB"}));
        assert!(!single.pack);
        // HDR beats a larger SDR pack, comparing pack sizes per episode within each HDR tier.
        let hdr = source_of("A", &json!({"url":"https://x.invalid/h","behaviorHints":{"filename":"Show.S01E01.2160p.WEB.DL.DV.HDR10P.H.265-GRP.mkv","videoSize":100813874330u64},
            "description":"Show.S01.2160p.WEB.DL.DV.HDR10P.H.265-GRP\nShow.S01E01.2160p.WEB.DL.DV.HDR10P.H.265-GRP.mkv\n💾93.89 GiB","name":"[Cached] StreamAddon 2160p\nHDR10+ | DV"}));
        assert_eq!(play_order_for(&[pack.clone(), hdr.clone()], Some(10), None)[0].url, hdr.url);
    }

    #[test]
    fn preferred_language_plays_first() {
        let big = source_of("A", &json!({"url":"https://x.invalid/a","title":"Film 2160p REMUX\n💾 60 GB\n🇬🇧"}));
        let ita = source_of("A", &json!({"url":"https://x.invalid/b","title":"Film 1080p WEB-DL ITA ENG\n💾 4 GB\n🇬🇧 / 🇮🇹"}));
        let list = vec![big.clone(), ita.clone()];
        assert_eq!(play_order_for(&list, None, None)[0].url, big.url);
        assert_eq!(play_order_for(&list, None, Some("italian"))[0].url, ita.url);
        // English: untagged sources count as English; the ITA ENG release has English too.
        assert_eq!(play_order_for(&list, None, Some("english"))[0].url, big.url);
        // An Italian-only 4K HDR10+ release goes after a smaller English one when you chose English.
        let ita_only = source_of("A", &json!({"url":"https://x.invalid/c","title":"Film 2160p WEB-DL HDR10+ ITA\n💾 30 GB\n🇮🇹"}));
        let eng = source_of("A", &json!({"url":"https://x.invalid/d","title":"Film 1080p WEB-DL\n💾 8 GB"}));
        let multi = source_of("A", &json!({"url":"https://x.invalid/e","title":"Film 2160p WEB-DL HDR10+ MULTi\n💾 32 GB\n🇮🇹 / 🇫🇷"}));
        let order: Vec<String> = play_order_for(&[ita_only.clone(), eng.clone()], None, Some("english")).into_iter().map(|s| s.url).collect();
        assert_eq!(order, [eng.url.clone(), ita_only.url.clone()]);
        assert_eq!(play_order_for(&[ita_only.clone(), eng.clone()], None, None)[0].url, ita_only.url);
        // A multi-audio release isn't "Italian only": it keeps its place for English.
        assert_eq!(play_order_for(&[eng.clone(), multi.clone()], None, Some("english"))[0].url, multi.url);
        // For Italian: Italian first, then untagged, then other languages only.
        let fr = source_of("A", &json!({"url":"https://x.invalid/f","title":"Film 2160p REMUX FRENCH\n💾 70 GB\n🇫🇷"}));
        let order: Vec<String> = play_order_for(&[fr.clone(), eng.clone(), ita_only.clone()], None, Some("italian")).into_iter().map(|s| s.url).collect();
        assert_eq!(order, [ita_only.url.clone(), eng.url.clone(), fr.url.clone()]);
    }

    #[test]
    fn all_stream_addons_rank_equally_by_quality_and_language() {
        let low = source_of("AddonA", &json!({"name":"AddonA 1080p","title":"Film.1080p.WEBRip.mkv","url":"https://x.invalid/a"}));
        let remux = source_of("AddonB", &json!({"name":"AddonB","title":"Film 2160p REMUX\n💾 60 GB","url":"https://x.invalid/b"}));
        let urls = |v: Vec<Source>| v.into_iter().map(|s| s.url).collect::<Vec<_>>();
        assert_eq!(urls(play_order_for(&[low.clone(), remux.clone()], None, None)),
            ["https://x.invalid/b", "https://x.invalid/a"]);
        // Chosen language still ranks first regardless of add-on name.
        let ita = source_of("AddonC", &json!({"name":"AddonC","title":"Film 1080P · 🇬🇧🇮🇹","url":"https://x.invalid/i"}));
        assert_eq!(urls(play_order_for(&[remux, ita], None, Some("italian")))[0], "https://x.invalid/i");
    }
    #[test]
    fn a_refusal_for_too_many_requests_says_so() {
        assert!(no_source_message(Some("busy")).contains("Try again in a minute"));
        assert!(no_source_message(None).starts_with("No source responded"));
    }

    #[test]
    fn audio_language_is_english_unless_chosen() {
        assert_eq!(audio_language(&json!({})), "english");
        assert_eq!(audio_language(&json!({"stream_language": null})), "english");
        assert_eq!(audio_language(&json!({"stream_language": "klingon"})), "english");
        assert_eq!(audio_language(&json!({"stream_language": "italian"})), "italian");
        assert_eq!(Settings::default().stream_language.as_deref(), Some("english"));
    }

    /// Cinema-filmed telesync/cam copies must never play, while dual-language titles still match.
    #[test]
    fn cinema_copies_never_play_and_translated_titles_count() {
        let rows = [("StreamAddon\n4k", "Project 3 2025 2160p WEBRip\n👤 32 💾 4.45 GB ⚙️ IndexerA"),
            ("StreamAddon\n4k", "Projekt 3 - Project 3 (2024)(CZ.EN)[2160p][WebDL] CZ-AAC2.0 EN-DDP5.1 SDR x265\n👤 20 💾 11.2 GB ⚙️ IndexerB 🇬🇧 / 🇨🇿"),
            ("StreamAddon\n1080p", "Proyecto 3 (Project 3) (2024) [Bluray 1080p][Esp]\n👤 2 💾 6.28 GB ⚙️ IndexerC 🇪🇸"),
            ("StreamAddon\nTeleSync", "Project 3 2025 1080p HDTS x264-GRP\n👤 9 💾 6.17 GB ⚙️ IndexerD")];
        let mut list: Vec<Source> = rows.iter().enumerate()
            .map(|(i, (n, t))| source_of("StreamAddon", &json!({"name": n, "title": t, "url": format!("https://x.invalid/{i}")}))).collect();
        for s in &mut list { s.off_title = !names_title(s, "Project 3", Some(2025)); }
        let order = play_order_for(&list, None, Some("english"));
        assert_eq!(order.iter().map(|s| s.size.as_deref().unwrap_or("")).collect::<Vec<_>>(), ["11.2 GB", "4.45 GB", "6.28 GB"],
            "the English 4K first; the telesync never");
        for camera in ["Film 2025 1080p HDTS x264", "Film.2025.2160p.HDCAM.x265", "Film 2025 1080p TELESYNC", "Film.2025.1080p.HD-TC.x264",
            "Film 2025 1080p CAMRip", "Film.2025.1080p.DVDSCR.x264", "Film 2025 2160p SCREENER"] {
            assert!(!playable(&source_of("A", &json!({"url":"https://x.invalid/c","title": camera}))), "{camera}");
        }
        // A bare TS is also the MPEG-TS container: a labelled release in one still plays.
        assert!(playable(&source_of("A", &json!({"url":"https://x.invalid/t","title":"Film.2025.2160p.UHD.BluRay.REMUX.HEVC.ts"}))));
    }

    #[test]
    fn releases_of_other_titles_go_last() {
        let src = |file: &str, description: &str| source_of("A", &json!({"url":"https://x.invalid/a","behaviorHints":{"filename":file},"title":description}));
        let other = src("Call.Me.by.Your.Name.2017.2160p.BluRay.REMUX.mkv", "Call.Me.by.Your.Name.2017.2160p.BluRay.REMUX.mkv\n💾 72 GB");
        assert!(!names_title(&other, "Your Name", Some(2016)));
        assert!(names_title(&src("Your Name 2016 2160p_DCPRip.mkv", ""), "Your Name", Some(2016)));
        assert!(names_title(&src("Your.Name.(2016).1080p.mkv", ""), "Your Name", Some(2016)));
        // The other film of that name, years apart.
        assert!(!names_title(&src("Dune.1984.2160p.REMUX.mkv", ""), "Dune", Some(2021)));
        assert!(names_title(&src("Dune.Part.Two.2024.2160p.mkv", ""), "Dune: Part Two", Some(2024)));
        // Series: the file names the show; the requested title alone on a line says nothing.
        assert!(names_title(&src("S02/The.Bear.S02E03.2160p.WEB.mkv", "The Bear"), "The Bear", None));
        assert!(!names_title(&src("Medvěd_S02E03_Pohár.mkv", "The Bear"), "The Bear", None));
        assert!(names_title(&src("", "www.1TamilMV.mx - The Bear (2023) S02 2160p\nEp 3"), "The Bear", None));
        assert!(names_title(&src("[Group] Amélie.2001.1080p.mkv", ""), "Amélie", Some(2001)));
        assert!(names_title(&src("Spider-Man.Across.the.Spider-Verse.2023.2160p.mkv", ""), "Spider-Man: Across the Spider-Verse", Some(2023)));
        // A longer title that starts with this one is another film or show.
        assert!(!names_title(&src("The.Dark.Knight.Rises.2012.IMAX.2160p.REMUX.mkv", ""), "The Dark Knight", Some(2008)));
        assert!(names_title(&src("The.Dark.Knight.2008.2160p.REMUX.mkv", ""), "The Dark Knight", Some(2008)));
        assert!(names_title(&src("The Dark Knight 2160p UHD BluRay.mkv", ""), "The Dark Knight", Some(2008)));
        assert!(!names_title(&src("Doctor.Who.Confidential.S01E01.mkv", ""), "Doctor Who", None));
        assert!(names_title(&src("Doctor.Who.2005.S01E01.1080p.mkv", ""), "Doctor Who", None));
        assert!(names_title(&src("Breaking Bad (2008) S01E01 (2160p).mkv", ""), "Breaking Bad", None));
        assert!(names_title(&src("Game.of.Thrones.S08E03.2160p.mkv", ""), "Game of Thrones", None));
        // Apostrophes and "&" are written either way in release names.
        assert!(names_title(&src("Schindlers.List.1993.2160p.UHD.BluRay.REMUX.mkv", ""), "Schindler's List", Some(1993)));
        assert!(names_title(&src("Schindler's List (1993) [1080p].mp4", ""), "Schindler\u{2019}s List", Some(1993)));
        assert!(names_title(&src("Fast.and.Furious.2009.1080p.mkv", ""), "Fast & Furious", Some(2009)));
        assert!(names_title(&src("Fast & Furious 2009 2160p.mkv", ""), "Fast & Furious", Some(2009)));
        // A translated title first, then the film's own.
        assert!(names_title(&src("", "Kód 3 - Code 3 (2024)(CZ.EN)[2160p][WebDL] CZ-AAC2.0 EN-DDP5.1 SDR x265"), "Code 3", Some(2025)));
        assert!(names_title(&src("", "Код 3 / Code 3 (2025) WEB-DL-HEVC 2160p"), "Code 3", Some(2025)));
        assert!(!names_title(&src("", "Code 37 - Season 1 (2011)"), "Code 3", Some(2025)));
        let mut native = src("Your.Name.2016.1080p.BluRay.mkv", "💾 8 GB");
        let mut off = other.clone();
        native.off_title = !names_title(&native, "Your Name", Some(2016));
        off.off_title = !names_title(&off, "Your Name", Some(2016));
        assert_eq!(play_order_for(&[off, native.clone()], None, None)[0].url, native.url);
    }

    #[test]
    fn the_original_remux_beats_a_bigger_dub_collection() {
        let src = |title: &str| source_of("A", &json!({"title": title, "url": format!("https://x.invalid/{}", title.len())}));
        let dubs = src("Film.1985.2160p.UHD.BluRay.REMUX.HDR.HEVC.22xRUS.UKR.ENG 💾 124 GB");
        let original = src("Film.1985.2160p.UHD.BluRay.REMUX.HDR.HEVC.TrueHD.7.1 💾 60 GB");
        let web = src("Film.1985.2160p.WEB-DL.HDR.HEVC 💾 20 GB");
        let order = play_order_for(&[web.clone(), original.clone(), dubs.clone()], None, None);
        assert_eq!(order[0].url, dubs.url);
        let urls = |r: Vec<(AudioVerdict, Source)>| r.into_iter().map(|(_, s)| s.url).collect::<Vec<_>>();
        // Both remuxes have English; the one with English as its main track goes first. Encodes keep their order.
        assert_eq!(urls(rank_by_audio(order.clone(), vec![Some((true, false)), Some((true, true)), Some((true, true))])), [original.url.clone(), dubs.url.clone(), web.url.clone()]);
        // A remux without English still goes after a WEB-DL with it.
        assert_eq!(urls(rank_by_audio(order.clone(), vec![Some((false, false)), Some((false, false)), Some((true, true))])), [web.url.clone(), dubs.url.clone(), original.url.clone()]);
        // Unread files keep the Play order.
        assert_eq!(urls(rank_by_audio(order, vec![None, None, None])), [dubs.url, original.url, web.url]);
    }

    #[test]
    fn upscales_go_after_native_files() {
        let src = |title: &str| source_of("A", &json!({"title": title, "url": format!("https://x.invalid/{}", title.len())}));
        let list = [src("Film.2001.BD.AI_UPSCALE_3840x2160 HDR 💾 57 GB"), src("Show.S01E01.2160p.Upscale.BluRay.DV.HDR 💾 13 GB"),
            src("Film.2016.2160p_DCPRip_4k_Upscale_DV_HDR10Plus 💾 83 GB"), src("Film.2001.1080p.BluRay.REMUX 💾 30 GB"),
            src("Film.2160p.SDR.to.HDR.WEB 💾 20 GB"), src("Film.2001.1080p.WEB-DL 💾 6 GB")];
        let order: Vec<String> = play_order_for(&list, None, None).into_iter().map(|s| s.label.trim().to_owned()).collect();
        assert_eq!(&order[..2], ["Film.2001.1080p.BluRay.REMUX 💾 30 GB", "Film.2001.1080p.WEB-DL 💾 6 GB"]);
        assert_eq!(order.len(), 6);
        for title in ["Upscaler.Documentary.2020.1080p", "Film.2160p.HDR.REMUX", "Film AI 2001 1080p"] { assert!(!upscaled().is_match(title), "{title}"); }
    }

    #[test]
    fn play_never_chooses_under_1080p() {
        let src = |title: &str| source_of("A", &json!({"title": title, "url": format!("https://x.invalid/{}", title.len())}));
        let list = [src("Film.720p.HDR.REMUX 💾 90 GB"), src("Film.480p.WEB 💾 2 GB"), src("Film.DVDRip 💾 9 GB"), src("Film.HDCAM 💾 3 GB"),
            src("Film HDR 💾 80 GB"), src("Film.1080p.WEB-DL 💾 4 GB"), src("Film.2160p.WEB-DL 💾 12 GB")];
        let order: Vec<String> = play_order_for(&list, None, None).into_iter().map(|s| s.label.trim().to_owned()).collect();
        // 720p, SD, DVD rips and cams never; a file without a resolution only after every labelled
        // one, even when it is bigger and HDR.
        assert_eq!(order, ["Film.2160p.WEB-DL 💾 12 GB", "Film.1080p.WEB-DL 💾 4 GB", "Film HDR 💾 80 GB"]);
    }

    #[test]
    fn play_order_matches_the_android_app() {
        let src = |title: &str| source_of("A", &json!({"title": title, "url": "https://x.invalid/a"}));
        let list = [src("Film.2160p.DV.WEB-DL 💾 30 GB"), src("Film.2160p.HDR10+.WEB-DL.HEVC 💾 20 GB"),
            src("Film.1080p.BluRay.x264 💾 40 GB"), src("Film.2160p.HDR.REMUX 💾 20.5 GB"),
            source_of("B", &json!({"title": "Film 4K 💾 90 GB", "infoHash": "abc"}))];
        let order: Vec<String> = play_order(&list, None).into_iter().map(|s| s.label.trim().to_owned()).collect();
        // HDR first, size before HDR10+; DV-only stays last; torrents never.
        assert_eq!(order, ["Film.2160p.HDR.REMUX 💾 20.5 GB", "Film.2160p.HDR10+.WEB-DL.HEVC 💾 20 GB",
            "Film.1080p.BluRay.x264 💾 40 GB", "Film.2160p.DV.WEB-DL 💾 30 GB"]);
        let pack = source_of("C", &json!({"title": "Show S01 COMPLETE 1080p 💾 40 GB", "url": "https://x.invalid/p"}));
        assert_eq!(episode_size(&pack, Some(10)), Some(4_000_000_000));
        assert_eq!(date_label("2014-11-05T00:00:00.000Z"), "5 Nov 2014");
    }

    /// What a file's header says outranks its label: a "4K HDR" that is a 1080p in SDR, a "HDR"
    /// that is Dolby Vision alone (profile 5), and a 4K whose real bitrate is a re-encode's.
    #[test]
    fn a_file_s_own_picture_outranks_its_label() {
        use crate::tracks::{Picture, Transfer};
        let src = |title: &str| source_of("A", &json!({"title": title, "url": format!("https://x.invalid/{}", title.len())}));
        let pic = |w, h, transfer, dv, seconds| Picture { width: w, height: h, transfer, dolby_vision: dv, seconds };
        let fake = with_picture(src("Film 2025 2160p HDR WEB-DL 30 GB"), pic(1920, 800, Transfer::Sdr, None, Some(7200.0)));
        assert_eq!((fake.quality, fake.hdr), (Some("1080p"), None));
        let real = with_picture(src("Film 2025 2160p WEB-DL 20 GB"), pic(3840, 1600, Transfer::Pq, None, Some(7200.0)));
        assert_eq!((real.quality, real.hdr), (Some("4K"), Some("HDR")), "HDR the label didn't say");
        let dv5 = with_picture(src("Film 2025 2160p HDR WEB-DL 25 GB"), pic(3840, 2160, Transfer::Pq, Some(5), Some(7200.0)));
        assert_eq!(dv5.hdr, Some("DV"));
        let plus = with_picture(src("Film 2025 2160p HDR10+ WEB-DL 22 GB"), pic(3840, 2160, Transfer::Pq, None, Some(7200.0)));
        assert_eq!(plus.hdr, Some("HDR10+"), "HDR10+ is in the frames: the label's stands over the header's HDR");
        let order: Vec<_> = play_order_for(&[fake.clone(), dv5.clone(), real.clone(), plus.clone()], None, None).into_iter().map(|s| s.url).collect();
        assert_eq!(order, [plus.url, real.url, fake.url, dv5.url]);
        // 2.5 GB over two hours is under 3 Mb/s: a 4K that competes as a 1080p, against a bigger 1080p.
        let thin = with_picture(src("Film 2025 2160p WEB-DL 2.5 GB"), pic(3840, 2160, Transfer::Sdr, None, Some(7200.0)));
        let full = src("Film 2025 1080p BluRay REMUX 30 GB");
        assert_eq!(play_order_for(&[thin.clone(), full.clone()], None, None)[0].url, full.url);
        assert_eq!(picture_summary(&thin), "3840x2160 SDR 3 Mb/s");
    }

    /// Resolution before size, but a file too small for its resolution competes as a 1080p.
    #[test]
    fn resolution_then_size_unless_the_file_is_too_small_for_it() {
        let src = |title: &str| source_of("A", &json!({"title": title, "url": format!("https://x.invalid/{}", title.len())}));
        let order = |list: &[Source]| play_order_for(list, None, None).into_iter().map(|s| s.label.trim().to_owned()).collect::<Vec<_>>();
        // A film: a 4.45 GB 4K WEBRip over a bigger 1080p; a 2 GB "4K" is ranked among the 1080p files by size.
        assert_eq!(order(&[src("Film 2025 1080p WEB-DL 6.28 GB"), src("Film 2025 2160p WEBRip 4.45 GB")]),
            ["Film 2025 2160p WEBRip 4.45 GB", "Film 2025 1080p WEB-DL 6.28 GB"]);
        assert_eq!(order(&[src("Film 2025 2160p WEBRip 2 GB"), src("Film 2025 1080p WEB-DL 6 GB"), src("Film 2025 1080p WEBRip 1.5 GB")]),
            ["Film 2025 1080p WEB-DL 6 GB", "Film 2025 2160p WEBRip 2 GB", "Film 2025 1080p WEBRip 1.5 GB"]);
        // An episode needs less: 1.5 GB is a real 4K episode, 0.6 GB isn't.
        assert_eq!(order(&[src("Show S01E01 1080p WEB-DL 3 GB"), src("Show S01E01 2160p WEB-DL 1.5 GB")]),
            ["Show S01E01 2160p WEB-DL 1.5 GB", "Show S01E01 1080p WEB-DL 3 GB"]);
        assert_eq!(order(&[src("Show S01E01 2160p WEBRip 0.6 GB"), src("Show S01E01 1080p WEB-DL 1 GB")]),
            ["Show S01E01 1080p WEB-DL 1 GB", "Show S01E01 2160p WEBRip 0.6 GB"]);
    }

    #[test]
    fn hdr_then_size_then_hdr_format() {
        let src = |title: &str| source_of("A", &json!({"title": title, "url": "https://x.invalid/a"}));
        let list = [src("Film 2160p HDR10+ WEB-DL 20 GB"), src("Film 2160p REMUX 60 GB"),
            src("Film 2160p HDR10+ WEB-DL"), src("Film 2160p HDR10+ REMUX 60 GB")];
        let order: Vec<_> = play_order_for(&list, None, None).into_iter().map(|s| s.label.trim().to_owned()).collect();
        assert_eq!(order, ["Film 2160p HDR10+ REMUX 60 GB", "Film 2160p HDR10+ WEB-DL 20 GB",
            "Film 2160p HDR10+ WEB-DL", "Film 2160p REMUX 60 GB"]);
    }

    #[test]
    fn exclude_3d_from_every_provider_and_metadata_field() {
        for tag in ["3D", "3-D", "SBS", "HSBS", "H-SBS", "Half.SBS", "Full_SBS", "HOU", "H-TAB", "TAB", "Side.by.Side", "Top.And.Bottom", "Over-Under", "Stereoscopic"] {
            assert!(is_3d(&format!("Show.S04E07.{tag}.2160p.mkv")), "{tag}");
        }
        for title in ["Three Days 2160p", "3 Days to Kill", "S03E03 1080p", "Film AC3 DDP5.1 HDR", "Film Stereotype"] {
            assert!(!is_3d(title), "{title}");
        }
        for addon in ["AddonA", "AddonB", "Other"] {
            for field in ["name", "title", "description", "filename"] {
                let mut v = json!({"url":"https://x.invalid/3d", "title":"Show S04E07 2160p HDR 90 GB"});
                if field == "filename" { v["behaviorHints"] = json!({"filename":"Show.S04E07.3D.HSBS.mkv"}); }
                else { v[field] = json!("Show S04E07 3D HDR 90 GB"); }
                assert!(play_order_for(&[source_of(addon, &v)], None, None).is_empty(), "{addon}/{field}");
            }
        }
        let src = |title: &str| source_of("AddonB", &json!({"title":title,"url":"https://x.invalid/2d"}));
        let small = src("Film 2160p HDR10+ WEB-DL 5 GB");
        let big = src("Film 2160p HDR REMUX 50 GB");
        let sdr = src("Film 2160p SDR REMUX 90 GB");
        let order: Vec<_> = play_order_for(&[sdr.clone(), small, big], None, None).into_iter().map(|s| s.size_bytes).collect();
        assert_eq!(order, [Some(50_000_000_000), Some(5_000_000_000), Some(90_000_000_000)]);
        assert_eq!(play_order_for(std::slice::from_ref(&sdr), None, None), [sdr]);
    }

    #[test]
    fn better_posters_first_then_the_addon_poster() {
        let m = Media::from_json(&json!({"id":"tt6262770","name":"Tenzing","type":"movie","poster":"https://images.example/p.jpg"}), "a").unwrap();
        assert_eq!(posters(&m), [
            "https://btttr.cc/poster/imdb/poster-default/tt6262770.jpg",
            "https://images.example/p.jpg",
            "https://images.metahub.space/poster/large/tt6262770/img",
            "https://images.metahub.space/poster/medium/tt6262770/img",
        ]);
        let k = Media::from_json(&json!({"id":"kitsu:1","name":"Anime","type":"anime","poster":"https://images.example/k.jpg"}), "a").unwrap();
        assert_eq!(posters(&k), ["https://images.example/k.jpg"]);
    }

    #[test]
    fn calendar_dates_and_relative_days() {
        assert_eq!(civil(0), (1970, 1, 1, 3));
        assert_eq!(civil(1_763_164_800), (2025, 11, 15, 5)); // a Saturday
        let now = 1_763_164_800 + 3600;
        assert_eq!(when_label(now - 60, now), "Today");
        assert_eq!(when_label(now - 86_400, now), "Yesterday");
        assert_eq!(when_label(now - 30 * 86_400, now), "16 Oct");
    }

    #[test]
    fn accent_is_readable_under_black_text_and_skips_greyscale() {
        let (r, g, b) = artwork_accent(&[[120, 20, 20, 255], [40, 40, 40, 255]]).unwrap();
        assert!(luminance(r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0) >= 0.17);
        assert!(artwork_accent(&[[128, 128, 128, 255]]).is_none());
    }

    #[test]
    fn hides_internal_raw_backing_remotes_when_crypt_companion_exists() {
        let mk = |name: &str, kind: &str| rstream_rclone::RemoteInfo {
            name: name.into(),
            kind: kind.into(),
            raw: json!({"type": kind}),
        };
        let visible = filter_visible_rclone_remotes(vec![
            mk("gdrive", "crypt"),
            mk("gdrive-raw", "drive"),
            mk("plain", "s3"),
            mk("orphan-raw", "webdav"),
        ]);
        let names: Vec<&str> = visible.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["gdrive", "plain", "orphan-raw"]);
    }
}

/// Real-network timing of the autoplay pipeline (add-ons → order → probe → first bytes).
/// Run against a copy of a store: `VEYLO_DX_DATA=/tmp/copy cargo test autoplay_bench -- --ignored --nocapture`.
/// Prints add-on names, labels' quality tags and timings only: never a URL.
#[cfg(test)]
mod autoplay_bench {
    use super::*;
    use std::time::Instant;

    /// kind/id/title/year (year for films).
    const TITLES: &[&str] = &["movie/tt15239678/Dune: Part Two/2024", "movie/tt15398776/Oppenheimer/2023", "movie/tt0133093/The Matrix/1999",
        "movie/tt0118799/Life Is Beautiful/1997", "series/tt0903747:1:1/Breaking Bad", "series/tt14452776:2:3/The Bear",
        "series/tt11280740:1:1/Severance", "movie/tt5311514/Your Name./2016", "movie/tt0211915/Amélie/2001",
        "series/tt0306414:1:1/The Wire", "movie/tt0034583/Casablanca/1942", "movie/tt6751668/Parasite/2019"];

    /// The backend on a copy of a store, once for every test here.
    fn start() {
        assert!(std::env::var_os("VEYLO_DX_DATA").is_some(), "point VEYLO_DX_DATA at a copy of a store");
        static STARTED: std::sync::Once = std::sync::Once::new();
        STARTED.call_once(|| init().unwrap());
    }

    async fn first_bytes(src: &Source, want: u64) -> Option<(u128, f64)> {
        let src = src.clone();
        on_runtime(async move {
            let client = reqwest::Client::builder().timeout(Duration::from_secs(20)).build().ok()?;
            let mut req = client.get(&src.url).header("range", format!("bytes=0-{}", want - 1));
            for (k, v) in &src.headers { req = req.header(k.as_str(), v.as_str()); }
            let t = Instant::now();
            let mut r = req.send().await.ok()?;
            let ttfb = t.elapsed().as_millis();
            let t2 = Instant::now();
            let mut got = 0u64;
            while let Ok(Some(chunk)) = r.chunk().await { got += chunk.len() as u64; if got >= want { break; } }
            Some((ttfb, got as f64 * 8.0 / 1e6 / t2.elapsed().as_secs_f64().max(0.001)))
        }).await.ok().flatten()
    }

    /// The app's developer benchmark (`dev_autoplay`) on BENCH_LIST: every title cold, then fetched ahead.
    #[test]
    #[ignore]
    fn dev_autoplay_list() {
        start();
        let list = std::fs::read_to_string(std::env::var("BENCH_LIST").expect("BENCH_LIST: a file of kind/id/title/year lines")).unwrap();
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(dev_autoplay(list));
    }

    #[test]
    #[ignore]
    fn autoplay_bench() {
        start();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let picked: Vec<String> = std::env::var("BENCH_TITLES").map(|t| t.split(',').map(str::to_owned).collect())
                .unwrap_or_else(|_| TITLES.iter().map(|t| t.to_string()).collect());
            for entry in &picked {
                let parts: Vec<&str> = entry.splitn(4, '/').collect();
                let (kind, id, name, year) = (parts[0], parts[1], parts.get(2).copied().unwrap_or(""), parts.get(3).and_then(|y| y.parse().ok()));
                println!("\n=== {kind} {id} {name}");
                let targets = exec(json!({"op":"streams_targets","type":kind,"id":id})).await.unwrap();
                for t in targets["targets"].as_array().unwrap().iter().filter(|_| std::env::var_os("BENCH_QUICK").is_none()) {
                    let t0 = Instant::now();
                    let r = exec(json!({"op":"streams","type":kind,"id":id,"addon_url":t["addon_url"],"force":true})).await;
                    let n = r.as_ref().ok().and_then(|v| v["groups"][0]["streams"].as_array().map(|a| a.len())).unwrap_or(0);
                    let err = r.as_ref().ok().and_then(|v| v["groups"][0]["error"].as_str().map(str::to_owned)).unwrap_or_default();
                    println!("  addon {:<28} {:>6} ms  {n:>3} streams {err}", t["addon_name"].as_str().unwrap_or(""), t0.elapsed().as_millis());
                }
                let pack = (kind == "series").then_some(10);
                if let Ok(lang) = std::env::var("BENCH_LANG") {
                    let (list, _) = sources(kind, id).await.unwrap();
                    for s in play_order_for(&list, pack, Some(&lang)).iter().take(14) {
                        let l = crate::addons::language(&lang).unwrap();
                        let tier = if l.matches(&s.label) { 0 } else if other_language_only(&s.label, l.key) { 2 } else { 1 };
                        println!("  [tier {tier}] {} {} {} | {}", s.quality.unwrap_or("?"), s.hdr.unwrap_or("SDR"), s.size.as_deref().unwrap_or("?"),
                            format!("{:?} // {:?}", s.name, s.description).chars().take(260).collect::<String>());
                    }
                    continue;
                }
                let t0 = Instant::now();
                let pick = autoplay(kind, id, pack, name, year).await;
                let took = t0.elapsed().as_millis();
                let rate = match &pick.best { Some(b) => first_bytes(b, 16 << 20).await.map(|(ttfb, mbps)| format!("ttfb {ttfb} ms, {mbps:.0} Mbit/s")).unwrap_or_else(|| "no data".into()), None => String::new() };
                println!("  AUTOPLAY {took} ms -> {} | {rate}", pick.best.as_ref().map(|b| format!("{} [lang {}] {} {:?}", source_summary(b), if claims_language(b) { "claimed" } else if other_language_only(&b.label, "english") { "OTHER" } else { "untagged" }, b.filename.chars().take(70).collect::<String>(), b.description.lines().last().unwrap_or(""))).unwrap_or_else(|| format!("nothing {:?}", pick.provider_status)));
                for r in pick.rest.iter().take(if std::env::var_os("BENCH_NAMES").is_some() { 4 } else { 0 }) {
                    println!("     next: {} {}", source_summary(r), r.filename.chars().take(70).collect::<String>());
                }
            }
        });
    }
}
