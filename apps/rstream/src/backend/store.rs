//! What the app keeps in its own folder besides the library: settings.json, tonight's Picks duel
//! (picks-duel.json) and the search history (search-history.json).
use super::*;

// ---------------------------------------------------------------- settings

/// Viewer settings kept in the app's own folder (settings.json).
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// BetterPosters (btttr.cc) first, the add-on's poster as fallback.
    pub better_posters: bool,
    /// TMDB v3 key for artwork; never logged or shown.
    pub tmdb_key: Option<String>,
    /// Preferred audio language for streams (a key of `crate::addons::LANGUAGES`): English unless
    /// chosen (there is no "any language"; settings saved without one read as English).
    pub stream_language: Option<String>,
    /// Windows: send HDR to the screen when it is in HDR (off: always tone-map to SDR).
    pub hdr_output: bool,
    /// Subtitle font scale (0.6..=2.0, default 1.05).
    pub sub_scale: f64,
    /// Subtitle vertical position from top in percent (75..=100, default 100).
    pub sub_pos: i32,
    /// Draw a subtle dark pill behind subtitles for legibility over bright scenes.
    pub sub_bg: bool,
    /// Automatically count down to the next episode at credits or end of episode.
    pub auto_next: bool,
    /// Block all BitTorrent P2P traffic unless a VPN tunnel or bound interface is active.
    pub torrent_killswitch: bool,
    /// Optional network interface name to bind BitTorrent sockets to (desktop `SO_BINDTODEVICE`, Android match).
    pub torrent_bind_interface: Option<String>,
    /// Default destination for Save actions: `None` saves locally, `Some(remote)` saves to an rclone remote.
    pub default_save_target: Option<String>,
    /// Concurrent active downloads (1..=6, default 3); cloud rclone uploads run on a separate pool.
    pub max_parallel_downloads: usize,
    /// Desktop: clicking the window close button hides RStream to the system tray instead of exiting.
    pub close_to_tray: bool,
    /// Optional rclone remote used to sync Library, watch history, progress, binge locks, and track prefs.
    pub sync_remote: Option<String>,
    /// Profile namespace under `RStream/.profiles/<sync_profile>/` so shared remotes keep personal histories separate.
    pub sync_profile: String,
}

/// Sanitize a profile folder slug (`[A-Za-z0-9_-]`, max 48 chars, defaulting to `"default"`).
pub fn sanitize_profile_name(raw: &str) -> String {
    let clean: String = raw
        .trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .chars()
        .take(48)
        .collect();
    if clean.is_empty() { "default".into() } else { clean }
}

/// The audio language when none was chosen, or the saved one is unknown.
pub const DEFAULT_LANGUAGE: &str = "english";

/// The saved audio language, English when there is none (older settings saved "Any language" as null).
pub(super) fn audio_language(saved: &Value) -> String {
    saved["stream_language"].as_str().filter(|l| crate::addons::language(l).is_some()).unwrap_or(DEFAULT_LANGUAGE).to_owned()
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            better_posters: true, tmdb_key: None,
            stream_language: Some(DEFAULT_LANGUAGE.into()), hdr_output: true,
            sub_scale: 1.05, sub_pos: 100, sub_bg: false, auto_next: true,
            torrent_killswitch: false, torrent_bind_interface: None, default_save_target: None,
            max_parallel_downloads: 3,
            close_to_tray: true,
            sync_remote: None,
            sync_profile: "default".into(),
        }
    }
}

pub(crate) fn parse_settings_value(v: &Value) -> Settings {
    Settings {
        better_posters: v["better_posters"].as_bool().unwrap_or(true),
        tmdb_key: v["tmdb_key"].as_str().map(str::trim).filter(|k| !k.is_empty()).map(str::to_owned),
        stream_language: Some(audio_language(v)),
        hdr_output: v["hdr_output"].as_bool().unwrap_or(true),
        sub_scale: v["sub_scale"].as_f64().unwrap_or(1.05).clamp(0.6, 2.0),
        sub_pos: v["sub_pos"].as_i64().unwrap_or(100).clamp(70, 100) as i32,
        sub_bg: v["sub_bg"].as_bool().unwrap_or(false),
        auto_next: v["auto_next"].as_bool().unwrap_or(true),
        torrent_killswitch: v["torrent_killswitch"].as_bool().unwrap_or(false),
        torrent_bind_interface: v["torrent_bind_interface"].as_str().map(str::trim).filter(|k| !k.is_empty()).map(str::to_owned),
        default_save_target: v["default_save_target"].as_str().map(str::trim).filter(|k| !k.is_empty()).map(str::to_owned),
        max_parallel_downloads: v["max_parallel_downloads"].as_u64().unwrap_or(3).clamp(1, 6) as usize,
        close_to_tray: v["close_to_tray"].as_bool().unwrap_or(true),
        sync_remote: v["sync_remote"].as_str().map(str::trim).filter(|k| !k.is_empty()).map(str::to_owned),
        sync_profile: v["sync_profile"].as_str().map(sanitize_profile_name).unwrap_or_else(|| "default".into()),
    }
}

pub fn load_settings() -> Settings {
    let v: Value = std::fs::read_to_string(downloads::data_root_for_db().join("settings.json")).ok()
        .and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    parse_settings_value(&v)
}

/// Atomically write a file that may hold credentials or tokens, restricting it to owner-only (0600) on Unix.
pub(crate) fn write_private_file(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let mut tmp_os = path.as_os_str().to_os_string();
    tmp_os.push(".tmp");
    let tmp = PathBuf::from(tmp_os);
    let _ = std::fs::remove_file(&tmp);
    let write_res = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if write_res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    write_res
}

pub fn save_settings(settings: &Settings) {
    let profile = sanitize_profile_name(&settings.sync_profile);
    let max_dl = settings.max_parallel_downloads.clamp(1, 6);
    let data = json!({
        "better_posters": settings.better_posters,
        "tmdb_key": settings.tmdb_key,
        "stream_language": settings.stream_language,
        "hdr_output": settings.hdr_output,
        "sub_scale": settings.sub_scale.clamp(0.6, 2.0),
        "sub_pos": settings.sub_pos.clamp(70, 100),
        "sub_bg": settings.sub_bg,
        "auto_next": settings.auto_next,
        "torrent_killswitch": settings.torrent_killswitch,
        "torrent_bind_interface": settings.torrent_bind_interface,
        "default_save_target": settings.default_save_target,
        "max_parallel_downloads": max_dl,
        "close_to_tray": settings.close_to_tray,
        "sync_remote": settings.sync_remote,
        "sync_profile": profile,
    }).to_string();
    let _ = write_private_file(&downloads::data_root_for_db().join("settings.json"), data.as_bytes());
    BETTER_POSTERS.store(settings.better_posters, std::sync::atomic::Ordering::Relaxed);
    HDR_OUTPUT.store(settings.hdr_output, std::sync::atomic::Ordering::Relaxed);
    super::apply_torrent_vpn_policy(settings);
    downloads::set_parallel_downloads_limit(max_dl);
    set_stream_language(settings.stream_language.clone());
    set_tmdb_key(settings.tmdb_key.clone());
    // The key and language decide which add-ons answer and with which links.
    forget_sources();
    if settings.sync_remote.is_some() {
        remote_db::trigger_profile_sync();
    }
}

// ---------------------------------------------------------------- per-title track preferences

/// Remembered audio and subtitle language for a series or film (`track-prefs.json`).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct TrackPref {
    pub audio_lang: Option<String>,
    pub sub_lang: Option<String>,
    pub sub_off: bool,
}

fn track_prefs_path() -> PathBuf { downloads::data_root_for_db().join("track-prefs.json") }

static TRACK_PREFS: Mutex<Option<HashMap<String, TrackPref>>> = Mutex::new(None);

fn read_track_prefs_from(path: &std::path::Path) -> HashMap<String, TrackPref> {
    let v: Value = std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    v.as_object().into_iter().flatten().map(|(k, entry)| {
        let pref = TrackPref {
            audio_lang: entry["audio"].as_str().filter(|s| !s.is_empty()).map(str::to_owned),
            sub_lang: entry["sub"].as_str().filter(|s| !s.is_empty()).map(str::to_owned),
            sub_off: entry["sub_off"].as_bool().unwrap_or(false),
        };
        (remote_db::canonicalize_track_pref_key(k), pref)
    }).collect()
}

fn write_track_prefs_json(map: &HashMap<String, TrackPref>) -> String {
    let obj: serde_json::Map<String, Value> = map.iter().map(|(k, p)| {
        (k.clone(), json!({"audio": p.audio_lang, "sub": p.sub_lang, "sub_off": p.sub_off}))
    }).collect();
    Value::Object(obj).to_string()
}

pub fn load_track_pref(media_key: &str) -> Option<TrackPref> {
    let canon = remote_db::canonicalize_track_pref_key(media_key);
    let mut guard = TRACK_PREFS.lock().ok()?;
    let map = guard.get_or_insert_with(|| {
        flush_writes();
        read_track_prefs_from(&track_prefs_path())
    });
    map.get(&canon).cloned()
}

pub fn save_track_pref(media_key: &str, update: impl FnOnce(&mut TrackPref)) {
    let canon = remote_db::canonicalize_track_pref_key(media_key);
    let saved_pref = {
        let Ok(mut guard) = TRACK_PREFS.lock() else { return };
        let map = guard.get_or_insert_with(|| {
            flush_writes();
            read_track_prefs_from(&track_prefs_path())
        });
        let entry = map.entry(canon.clone()).or_default();
        update(entry);
        let cloned = entry.clone();
        // Keep the file bounded to 400 recent titles.
        if map.len() > 400 {
            let drop_key = map.keys().find(|k| k.as_str() != canon).cloned();
            if let Some(k) = drop_key { map.remove(&k); }
        }
        write_soon(track_prefs_path(), write_track_prefs_json(map));
        cloned
    };
    remote_db::queue_track_pref_sync(&canon, &saved_pref);
}

pub(crate) fn all_track_prefs_snapshot() -> HashMap<String, TrackPref> {
    let Ok(mut guard) = TRACK_PREFS.lock() else { return HashMap::new() };
    guard
        .get_or_insert_with(|| {
            flush_writes();
            read_track_prefs_from(&track_prefs_path())
        })
        .clone()
}

pub(crate) fn apply_synced_track_pref(media_key: &str, pref: TrackPref) {
    let canon = remote_db::canonicalize_track_pref_key(media_key);
    let Ok(mut guard) = TRACK_PREFS.lock() else { return };
    let map = guard.get_or_insert_with(|| {
        flush_writes();
        read_track_prefs_from(&track_prefs_path())
    });
    map.insert(canon.clone(), pref);
    if map.len() > 400 {
        let drop_key = map.keys().find(|k| k.as_str() != canon).cloned();
        if let Some(k) = drop_key { map.remove(&k); }
    }
    write_soon(track_prefs_path(), write_track_prefs_json(map));
}

// ---------------------------------------------------------------- redacted diagnostics

static DIAG_RING: Mutex<std::collections::VecDeque<String>> = Mutex::new(std::collections::VecDeque::new());

/// Strip URLs, API keys and tokens from diagnostic lines so logs are always safe to share.
pub fn redact_diagnostic(line: &str) -> String {
    static_re!(scheme_re, r"(?i)\b[a-z][a-z0-9+.-]*://[^\s\x22'<>]+");
    static_re!(auth_hdr_re, r#"(?i)"?(?:proxy-)?authorization"?\s*[:=]\s*(?:"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|(?:Bearer|Basic|Digest|Token)\s+[^\s&,\x22';})\]]+|[^\s&,\x22';})\]]+)"#);
    static_re!(bearer_re, r"(?i)\bBearer\s+[A-Za-z0-9._~+/=-]{8,}");
    static_re!(dq_secret_re, r#"(?i)("?(?:[a-z0-9_]*[_-])?(?:api[_-]?key|token|secret|password2?|pass|passwd|rd_key|tmdb_key|secret[_-]?access[_-]?key)"?\s*[:=]\s*)"(?:\\.|[^"\\])*""#);
    static_re!(sq_secret_re, r"(?i)('?(?:[a-z0-9_]*[_-])?(?:api[_-]?key|token|secret|password2?|pass|passwd|rd_key|tmdb_key|secret[_-]?access[_-]?key)'?\s*[:=]\s*)'(?:\\.|[^'\\])*'");
    static_re!(kv_secret_re, r"(?i)\b((?:[a-z0-9_]*[_-])?(?:api[_-]?key|token|secret|password2?|pass|passwd|rd_key|tmdb_key|secret[_-]?access[_-]?key))\s*[:=]\s*[^\s&,\x22';})\]]+");
    let s1 = scheme_re().replace_all(line, "[URL]");
    let s2 = auth_hdr_re().replace_all(&s1, "Authorization=[REDACTED]");
    let s3 = bearer_re().replace_all(&s2, "Bearer [REDACTED]");
    let s4 = dq_secret_re().replace_all(&s3, "$1\"[REDACTED]\"");
    let s5 = sq_secret_re().replace_all(&s4, "$1'[REDACTED]'");
    kv_secret_re().replace_all(&s5, "$1=[REDACTED]").into_owned()
}

pub fn record_diag_line(message: &str) {
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let clean = redact_diagnostic(message);
    if let Ok(mut q) = DIAG_RING.lock() {
        if q.len() >= 200 { q.pop_front(); }
        q.push_back(format!("{ms} {clean}"));
    }
}

/// A shareable, redacted diagnostic snapshot (version, OS/arch, HDR screen, recent events, last panic).
pub fn diagnostic_report() -> String {
    let mut out = String::new();
    let settings = load_settings();
    let (screen_title, _) = crate::display::summary(crate::display::screen(), settings.hdr_output);
    out.push_str(&format!("RStream {} ({}/{})\n", env!("CARGO_PKG_VERSION"), std::env::consts::OS, std::env::consts::ARCH));
    out.push_str(&format!("Screen: {screen_title}\n"));
    out.push_str(&format!("Language: {} | TMDB key: {}\n",
        settings.stream_language.as_deref().unwrap_or(DEFAULT_LANGUAGE),
        if settings.tmdb_key.is_some() { "set" } else { "none" },
    ));
    out.push_str("--- Recent events ---\n");
    let mut lines: Vec<String> = Vec::new();
    if let Ok(raw) = std::fs::read_to_string(backend().data_dir.join("dev-log.txt")) {
        for l in raw.lines().rev().take(120).collect::<Vec<_>>().into_iter().rev() {
            lines.push(redact_diagnostic(l));
        }
    }
    if lines.is_empty() {
        if let Ok(q) = DIAG_RING.lock() { lines.extend(q.iter().cloned()); }
    }
    if lines.is_empty() { out.push_str("(no recent events)\n"); }
    else { for l in lines { out.push_str(&l); out.push('\n'); } }
    if let Ok(panic_text) = std::fs::read_to_string(backend().data_dir.join("last-panic.txt")) {
        let snippet: String = panic_text.lines().take(25).map(redact_diagnostic).collect::<Vec<_>>().join("\n");
        if !snippet.trim().is_empty() {
            out.push_str("--- Last panic ---\n");
            out.push_str(&snippet);
            out.push('\n');
        }
    }
    out
}

// ---------------------------------------------------------------- watch-history import & export (Trakt / IMDb / Veylo)

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedTitle {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub year: String,
    pub watched: bool,
    pub saved: bool,
    pub liked: i64,
}

/// Parse a Trakt watched JSON export (`[{"movie":{...}}]` / `[{"show":{...}}]`), a Veylo JSON
/// export (`{"veylo_export":1,"items":[...]}`), or an IMDb CSV / plain list of `tt...` IDs.
pub fn parse_watch_history_import(raw: &str) -> Vec<ImportedTitle> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    fn push_item(out: &mut Vec<ImportedTitle>, seen: &mut HashSet<String>, item: ImportedTitle) {
        if item.id.is_empty() { return; }
        let k = format!("{}|{}", item.kind, item.id);
        if seen.insert(k) { out.push(item); }
    }

    if let Ok(v) = serde_json::from_str::<Value>(raw.trim()) {
        // Veylo JSON export
        if let Some(items) = v.get("items").and_then(Value::as_array) {
            for e in items {
                let id = s(e, "id");
                let kind = Some(s(e, "type")).filter(|k| !k.is_empty()).unwrap_or_else(|| "movie".into());
                let name = Some(s(e, "name")).filter(|n| !n.is_empty()).unwrap_or_else(|| id.clone());
                let year = s(e, "year");
                let watched = e["watched"].as_bool().unwrap_or(true);
                let saved = e["saved"].as_bool().unwrap_or(false);
                let liked = e["liked"].as_i64().unwrap_or(0).clamp(-1, 1);
                push_item(&mut out, &mut seen, ImportedTitle { id, kind, name, year, watched, saved, liked });
            }
            return out;
        }
        // Trakt watched / history / watchlist JSON array
        if let Some(arr) = v.as_array() {
            for e in arr {
                if let Some(m) = e.get("movie") {
                    let id = s(&m["ids"], "imdb");
                    let name = Some(s(m, "title")).filter(|n| !n.is_empty()).unwrap_or_else(|| id.clone());
                    let year = m["year"].as_i64().map(|y| y.to_string()).unwrap_or_default();
                    let saved = e.get("listed_at").is_some();
                    push_item(&mut out, &mut seen, ImportedTitle { id, kind: "movie".into(), name, year, watched: !saved, saved, liked: 0 });
                } else if let Some(sh) = e.get("show") {
                    let id = s(&sh["ids"], "imdb");
                    let name = Some(s(sh, "title")).filter(|n| !n.is_empty()).unwrap_or_else(|| id.clone());
                    let year = sh["year"].as_i64().map(|y| y.to_string()).unwrap_or_default();
                    let saved = e.get("listed_at").is_some();
                    push_item(&mut out, &mut seen, ImportedTitle { id, kind: "series".into(), name, year, watched: !saved, saved, liked: 0 });
                }
            }
            if !out.is_empty() { return out; }
        }
    }

    // Fallback: IMDb CSV export or plain text containing `tt1234567` IDs.
    static_re!(imdb_re, r"\b(tt\d{7,9})\b");
    for line in raw.lines() {
        let Some(caps) = imdb_re().captures(line) else { continue };
        let id = caps[1].to_owned();
        let lower = line.to_lowercase();
        let kind = if lower.contains("tv series") || lower.contains("tv mini") || lower.contains("series") { "series" } else { "movie" }.to_owned();
        // Best-effort title from CSV if present (column after id or quoted title).
        let name = line.split(',').nth(5)
            .map(|c| c.trim().trim_matches('"').to_owned())
            .filter(|n| !n.is_empty() && !n.starts_with("http"))
            .unwrap_or_else(|| id.clone());
        push_item(&mut out, &mut seen, ImportedTitle { id, kind, name, year: String::new(), watched: true, saved: false, liked: 0 });
    }
    out
}

/// Export watched, saved, and liked titles as a portable JSON backup (zero keys or addon secrets).
pub async fn export_watch_history() -> Result<String> {
    let items = library().await?;
    let rows: Vec<Value> = items.into_iter().filter(|m| m.watched || m.saved || m.liked != 0 || m.position_ms > 0).map(|m| json!({
        "id": m.id,
        "type": m.kind,
        "name": m.name,
        "year": m.year,
        "watched": m.watched,
        "saved": m.saved,
        "liked": m.liked,
    })).collect();
    let payload = json!({"rstream_export": 1, "exported_at": now_secs(), "items": rows});
    let text = serde_json::to_string_pretty(&payload)?;
    let _ = write_private_file(&backend().data_dir.join("rstream-history-export.json"), text.as_bytes());
    Ok(text)
}

/// Import titles from Trakt JSON, RStream/Veylo JSON, or IMDb CSV into the local library.
pub async fn import_watch_history(raw: &str) -> Result<usize> {
    let parsed = parse_watch_history_import(raw);
    anyhow::ensure!(!parsed.is_empty(), "No titles found. Paste a Trakt watched JSON, RStream export JSON, or IMDb CSV.");
    let mut count = 0usize;
    for item in parsed {
        let meta = json!({
            "id": item.id,
            "type": item.kind,
            "name": item.name,
            "releaseInfo": item.year,
        });
        let Some(media) = Media::from_json(&meta, CINEMETA) else { continue };
        if item.watched { let _ = set_watched(&media, true).await; }
        if item.saved { let _ = set_saved(&media, true).await; }
        if item.liked != 0 { let _ = feedback(&media, item.liked).await; }
        count += 1;
    }
    remote_db::trigger_profile_sync();
    Ok(count)
}

/// Change one or more settings: read, changed, saved and applied; returns what was saved.
pub fn update_settings(change: impl FnOnce(&mut Settings)) -> Settings {
    let mut settings = load_settings();
    change(&mut settings);
    save_settings(&settings);
    settings
}

/// Whether TMDB accepts a v3 API key (a cheap request that needs one).
pub async fn check_tmdb_key(key: String) -> Result<()> {
    on_runtime(async move {
        let response = backend().http.get("https://api.themoviedb.org/3/configuration").query(&[("api_key", key)]).send().await
            .map_err(|_| anyhow!("TMDB didn't answer. Check your connection."))?;
        anyhow::ensure!(response.status().is_success(), "TMDB didn't accept this key. Use the API Key, not the Read Access Token.");
        Ok(())
    }).await?
}

pub(super) fn media_json(m: &Media) -> Value {
    let mut o: serde_json::Map<String, Value> = m.raw.as_object().into_iter().flatten()
        .filter(|(k, _)| k.as_str() != "videos").map(|(k, v)| (k.clone(), v.clone())).collect();
    for (k, x) in [("id", &m.id), ("type", &m.kind), ("name", &m.name), ("addon_url", &m.addon_url), ("poster", &m.poster),
        ("background", &m.background), ("description", &m.description), ("releaseInfo", &m.year)] { o.insert(k.into(), json!(x)); }
    o.insert("genres".into(), json!(m.genres));
    Value::Object(o)
}

/// Files written behind the UI: each keeps only its latest content, and a later write always
/// lands after an earlier one. For state saved often that nothing reads back at once.
fn write_soon(path: std::path::PathBuf, text: String) {
    let first = PENDING_WRITES.lock().map(|mut p| { let map = p.get_or_insert_with(HashMap::new); let first = map.is_empty(); map.insert(path, text); first }).unwrap_or(false);
    if first { std::thread::spawn(|| { std::thread::sleep(Duration::from_millis(200)); flush_writes(); }); }
}
static PENDING_WRITES: Mutex<Option<HashMap<std::path::PathBuf, String>>> = Mutex::new(None);
static WRITING: Mutex<()> = Mutex::new(());

/// Write what `write_soon` still holds, now: before a file is read back or the app exits.
pub fn flush_writes() {
    let _turn = WRITING.lock();
    let batch = PENDING_WRITES.lock().ok().and_then(|mut p| p.take()).unwrap_or_default();
    for (path, text) in batch { let _ = write_private_file(&path, text.as_bytes()); }
}

// ---------------------------------------------------------------- search history

/// App-private search history: recorded on submit or opening a result, never per keystroke.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct History {
    pub queries: Vec<String>,
    pub items: Vec<Media>,
}

pub(super) fn history_path() -> PathBuf {
    backend().data_dir.join("search-history.json")
}

pub fn load_history() -> History {
    flush_writes();
    let Ok(text) = std::fs::read_to_string(history_path()) else { return History::default() };
    let Ok(v) = serde_json::from_str::<Value>(&text) else { return History::default() };
    History {
        queries: v["queries"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect(),
        items: v["items"].as_array().into_iter().flatten().filter_map(|m| Media::from_json(m, "")).collect(),
    }
}

pub(super) fn save_history(h: &History) {
    let items: Vec<Value> = h.items.iter().map(|m| {
        let mut raw: serde_json::Map<String, Value> = m.raw.as_object().into_iter().flatten()
            .filter(|(k, _)| k.as_str() != "videos").map(|(k, v)| (k.clone(), v.clone())).collect();
        raw.insert("addon_url".into(), json!(m.addon_url));
        if let Some(Value::String(d)) = raw.get_mut("description") { d.truncate(d.char_indices().nth(1000).map_or(d.len(), |(i, _)| i)); }
        Value::Object(raw)
    }).collect();
    write_soon(history_path(), json!({"queries":h.queries,"items":items}).to_string());
}

pub fn recent_queries(query: &str, previous: &[String]) -> Vec<String> {
    let trimmed: String = query.trim().chars().take(200).collect();
    if trimmed.chars().count() < 2 { return previous.to_vec(); }
    let mut out = vec![trimmed];
    for p in previous {
        if !out.iter().any(|x| x.to_lowercase() == p.to_lowercase()) { out.push(p.clone()); }
    }
    out.truncate(HISTORY_LIMIT);
    out
}

pub fn remember_query(h: &mut History, query: &str) {
    h.queries = recent_queries(query, &h.queries);
    save_history(h);
}

pub fn remember_item(h: &mut History, media: &Media) {
    let mut items = vec![media.clone()];
    items.extend(h.items.iter().filter(|m| m.key() != media.key()).cloned());
    items.truncate(HISTORY_LIMIT);
    h.items = items;
    save_history(h);
}

pub fn clear_history(h: &mut History) {
    *h = History::default();
    // Through the same writer, so a save still pending can't bring the old history back.
    save_history(h);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_private_file_enforces_owner_only_permissions_on_unix() {
        let dir = std::env::temp_dir().join(format!("veylo-store-perm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("secret.json");
        std::fs::create_dir_all(&dir).unwrap();
        // Start with world-readable 0644 and verify write_private_file tightens it to 0600.
        std::fs::write(&path, "old").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        write_private_file(&path, b"{\"key\":\"top-secret\"}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"key\":\"top-secret\"}");
        assert!(!dir.join("secret.json.tmp").exists(), "atomic temp file must be renamed away");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "secret file mode was {mode:o}, expected 0600");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_soon_coalesces_and_flush_writes_persists_immediately() {
        let dir = std::env::temp_dir().join(format!("veylo-store-flush-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        for i in 1..=20 {
            write_soon(path.clone(), format!("{{\"rev\":{i}}}"));
        }
        flush_writes();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"rev\":20}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "flush_writes must write 0600 files");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn track_prefs_roundtrip_and_redaction() {
        let dir = std::env::temp_dir().join(format!("veylo-track-prefs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("track-prefs.json");
        let mut map = HashMap::new();
        map.insert(
            "series|tt0903747|https://v3-cinemeta.strem.io/manifest.json".into(),
            TrackPref { audio_lang: Some("jpn".into()), sub_lang: Some("eng".into()), sub_off: false },
        );
        std::fs::write(&path, write_track_prefs_json(&map)).unwrap();
        assert_eq!(read_track_prefs_from(&path), map);
        // Legacy custom addon_url keys canonicalize on read without leaking addon URLs.
        std::fs::write(
            &path,
            r#"{"series|tt0903747|https://secret.addon/tok/manifest.json":{"audio":"jpn","sub":"eng","sub_off":false}}"#,
        )
        .unwrap();
        assert_eq!(read_track_prefs_from(&path), map);
        let _ = std::fs::remove_dir_all(&dir);

        let clean = redact_diagnostic(
            r#"fetch https://addon.example/cfg=SECRET/manifest.json sftp://u:pw1@nas/p stremio://x.io/sec/manifest.json api_key=12345 token="quot-sec" pass='sq-sec' {"access_token":"ya29.tok","pass":"json-pw"} Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.sig"#,
        );
        for secret in ["SECRET", "pw1", "sec/manifest", "12345", "quot-sec", "sq-sec", "ya29.tok", "json-pw", "eyJhbGci"] {
            assert!(!clean.contains(secret), "leaked `{secret}` in `{clean}`");
        }
        assert!(clean.contains("[URL]") && clean.contains("[REDACTED]"), "{clean}");
    }

    #[test]
    fn parses_trakt_veylo_and_imdb_history_imports() {
        let trakt = r#"[
            {"plays":1,"movie":{"title":"Dune","year":2021,"ids":{"imdb":"tt1160419"}}},
            {"plays":2,"show":{"title":"Severance","year":2022,"ids":{"imdb":"tt11280740"}}}
        ]"#;
        let t = parse_watch_history_import(trakt);
        assert_eq!(t.len(), 2);
        assert_eq!((t[0].id.as_str(), t[0].kind.as_str(), t[0].name.as_str(), t[0].watched), ("tt1160419", "movie", "Dune", true));
        assert_eq!((t[1].id.as_str(), t[1].kind.as_str(), t[1].name.as_str()), ("tt11280740", "series", "Severance"));

        let veylo = r#"{"veylo_export":1,"items":[{"id":"tt0133093","type":"movie","name":"The Matrix","year":"1999","watched":true,"saved":true,"liked":1}]}"#;
        let v = parse_watch_history_import(veylo);
        assert_eq!(v.len(), 1);
        assert_eq!((v[0].id.as_str(), v[0].saved, v[0].liked), ("tt0133093", true, 1));

        let csv = "Const,Your Rating,Date Rated,Title Type,URL,Title\ntt0468569,10,2024-01-01,Movie,https://imdb.com,The Dark Knight\ntt0903747,10,2024-01-02,TV Series,https://imdb.com,Breaking Bad\n";
        let c = parse_watch_history_import(csv);
        assert_eq!(c.len(), 2);
        assert_eq!((c[0].id.as_str(), c[0].kind.as_str(), c[0].name.as_str()), ("tt0468569", "movie", "The Dark Knight"));
        assert_eq!((c[1].id.as_str(), c[1].kind.as_str(), c[1].name.as_str()), ("tt0903747", "series", "Breaking Bad"));
    }
}
